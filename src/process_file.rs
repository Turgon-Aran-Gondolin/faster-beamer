//
// process_file.rs
// Copyright (C) 2019 seitz_local <seitz_local@lmeXX>
// Distributed under terms of the GPLv3 license.
//
use crate::beamer::get_frames;
use crate::fs_utils::{cache_path, configured_cache_dir, publish_file};
use crate::latexcompile::{BibliographyTool, LatexEngine};
use crate::parsing;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use log::Level::Trace;

use crate::latexcompile::{summarize_command_output, LatexCompiler, LatexInput, LatexRunOptions};
use clap::ArgMatches;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::env::current_dir;
use std::fs;
use std::fs::write;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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

#[derive(Clone)]
struct DocumentContextSnippet {
    content: String,
    source_start_line: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FrameLabel {
    Title,
    Toc,
    Number(usize),
}

impl FrameLabel {
    fn progress_label(&self) -> String {
        match self {
            FrameLabel::Title => String::from("title"),
            FrameLabel::Toc => String::from("toc"),
            FrameLabel::Number(number) => number.to_string(),
        }
    }

    fn status_label(&self, numbered_frame_count: usize) -> String {
        match self {
            FrameLabel::Title => String::from("title"),
            FrameLabel::Toc => String::from("toc"),
            FrameLabel::Number(number) => {
                if numbered_frame_count == 0 {
                    number.to_string()
                } else {
                    format!("{}/{}", number, numbered_frame_count)
                }
            }
        }
    }

    fn counter_value(&self) -> usize {
        match self {
            FrameLabel::Number(number) => number.saturating_sub(1),
            FrameLabel::Title | FrameLabel::Toc => 0,
        }
    }
}

struct UnitedCompileArtifacts {
    tex_file: PathBuf,
    pdf_file: PathBuf,
    sync_map: FrameSyncTexMap,
}

struct ParsedSyncTex {
    header_lines: Vec<String>,
    input_lines: Vec<(u32, String)>,
    content_lines: Vec<String>,
    record_count: usize,
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
        Regex::new(r"(?ms)^[ \t]*\\begin\{frame\}.*?^[ \t]*\\end\{frame\}|^[ \t]*\\sectiontitlepage\{[^{}]*(?:\{[^{}]*\}[^{}]*)*\}\{[^{}]*(?:\{[^{}]*\}[^{}]*)*\}|^[ \t]*\\titlepage\b|^[ \t]*\\includepdf\b(?:[ \t]*\[[^\]]*\])?[ \t]*\{[^{}]*\}").unwrap();
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
    static ref TITLE_PAGE_REGEX: Regex = Regex::new(r"\\(?:titlepage|maketitle)\b").unwrap();
}
lazy_static! {
    static ref DYNAMIC_TOC_OPTION_REGEX: Regex =
        Regex::new(r"\\tableofcontents\s*\[[^\]]*(?:currentsection|currentsubsection)[^\]]*\]")
            .unwrap();
}
lazy_static! {
    static ref DOCUMENT_REGEX: Regex =
        Regex::new(r"(?ms)^[ \t]*\\begin\{document\}.*^[ \t]*\\end\{document\}").unwrap();
}

lazy_static! {
    static ref RELATED_FILE_REGEX: Regex = Regex::new(
        r"(?sx)
        \\(?P<command>includegraphics|includepdf|input|include)
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
const PDFUNITE_TEMP_FILE: &str = "faster-beamer-pdfunite-output.pdf";
const CACHE_GARBAGE_RETENTION_DAYS: u64 = 30;
const CACHE_GARBAGE_SWEEP_INTERVAL_HOURS: u64 = 24;
const CACHE_GARBAGE_SWEEP_STAMP: &str = ".last-garbage-sweep";
const LUALATEX_AUTO_PARALLEL_JOBS: usize = 3;
const GRAPHICS_EXTENSIONS: [&str; 6] = ["pdf", "png", "jpg", "jpeg", "eps", "svg"];
const DEPENDENCY_MANIFEST_EXTENSION: &str = "deps";
const FRAME_JOB_SIDECAR_EXTENSIONS: [&str; 13] = [
    "aux",
    "bcf",
    "bbl",
    "blg",
    "fls",
    "log",
    "nav",
    "out",
    "run.xml",
    "snm",
    "synctex.gz",
    "toc",
    "vrb",
];

fn frame_counter_setup(frame_label: &FrameLabel, correct_frame_numbers: bool) -> String {
    if correct_frame_numbers {
        format!(
            "\\setcounter{{framenumber}}{{{}}}\n",
            frame_label.counter_value()
        )
    } else {
        String::new()
    }
}

fn frame_number_display_setup(frame_label: &FrameLabel) -> &'static str {
    match frame_label {
        FrameLabel::Toc => {
            "\\makeatletter\n\
\\def\\insertframenumber{}\n\
\\setbeamertemplate{page number in head/foot}{}\n\
\\setbeamertemplate{frame numbering}{}\n\
\\makeatother\n"
        }
        FrameLabel::Title | FrameLabel::Number(_) => "",
    }
}

fn numbered_frame_count(frame_labels: &[FrameLabel]) -> usize {
    frame_labels
        .iter()
        .filter(|label| matches!(label, FrameLabel::Number(_)))
        .count()
}

fn frame_label_for_index(
    frame_labels: &[FrameLabel],
    frame_idx: usize,
    numbered_frame_count: usize,
) -> String {
    frame_labels
        .get(frame_idx)
        .map(|label| label.status_label(numbered_frame_count))
        .unwrap_or_else(|| format!("raw frame {}", frame_idx + 1))
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
    cache_subdir: &Path,
    frame_labels: &[FrameLabel],
    numbered_frame_count: usize,
) {
    let source_log = source_file.with_extension("log");
    error!(
        "Compilation aborted: {} frame build(s) failed. Details were written to {}.",
        failures.len(),
        source_log.display()
    );

    for failure in failures {
        let source_end_line = failure
            .source_start_line
            .saturating_add(failure.source_line_count.saturating_sub(1));
        let frame_log = cache_subdir.join(Path::new(&failure.temp_file_name).with_extension("log"));

        error!(
            "Frame {} failed at {}:{}-{}.",
            frame_label_for_index(frame_labels, failure.frame_idx, numbered_frame_count),
            source_file.display(),
            failure.source_start_line,
            source_end_line
        );
        error!("Reason: {}", failure.error);
        error!("Generated source: {}", failure.temp_file.display());
        error!("Frame log: {}", frame_log.display());

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

fn remap_frame_log_to_source(
    failure: &FrameCompileFailure,
    source_file_name: &str,
    log_content: &str,
) -> String {
    let mut remapped = log_content.replace(&failure.temp_file_name, source_file_name);
    remapped = remapped.replace(
        failure.temp_file.to_string_lossy().as_ref(),
        source_file_name,
    );

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
        error!(
            "Failed to write master log {}: {}",
            source_log.display(),
            err
        );
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
    frame_labels: &[FrameLabel],
    numbered_frame_count: usize,
) -> Result<()> {
    let source_file_name = tex_input_name(source_file);
    let mut master_log = String::new();

    master_log.push_str("This is faster-beamer, aggregated frame failure log.\n");
    master_log.push_str(&format!("(./{})\n", source_file_name));

    for failure in failures {
        let log_path = cache_subdir.join(Path::new(&failure.temp_file_name).with_extension("log"));

        master_log.push_str(&format!(
            "! faster-beamer: frame {} failed to compile\n",
            frame_label_for_index(frame_labels, failure.frame_idx, numbered_frame_count)
        ));
        master_log.push_str(&format!(
            "l.{} {}\n",
            failure.source_start_line, failure.frame_preview
        ));

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
    let style = ProgressStyle::with_template("Compile {pos}/{len} jobs [{msg}]")
        .expect("compile progress bar template should be valid");
    progress_bar.set_style(style);
    progress_bar.set_message(".".repeat(total_jobs));
    progress_bar
}

fn render_frame_map(map: &[(char, FrameLabel)]) -> String {
    map.iter()
        .map(|(status, frame_label)| format!("{}{}", frame_label.progress_label(), status))
        .collect::<Vec<_>>()
        .join(" ")
}

fn show_error_slide(cachedir: &Path, output_file: &str, latex_engine: LatexEngine) {
    let error_frame = String::from_utf8_lossy(include_bytes!("error.tex")).to_owned();
    let error_file = cachedir.join("error.tex");
    let error_pdf = cachedir.join("error.pdf");

    if !error_pdf.exists() && write(&error_file, &error_frame[..]).is_ok() {
        let compiler = LatexCompiler::new_in_with_engine(cachedir.to_owned(), latex_engine)
            .add_arg("-shell-escape")
            .add_arg("-interaction=nonstopmode");

        let _result = compiler.run(&error_file, &LatexInput::new(), LatexRunOptions::new());
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

fn lualatex_format_dump_completed_after_backend_error(format_path: &Path, log_path: &Path) -> bool {
    if !format_path
        .metadata()
        .map(|metadata| metadata.is_file() && metadata.len() > 0)
        .unwrap_or(false)
    {
        return false;
    }

    fs::read_to_string(log_path)
        .map(|log| {
            log.contains("Beginning to dump on file")
                && log.contains("(pdf backend): already written content discarded")
        })
        .unwrap_or(false)
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

fn read_synctex_contents(synctex_file: &Path) -> std::result::Result<String, String> {
    let compressed = std::fs::read(synctex_file)
        .map_err(|err| format!("failed to read {}: {}", synctex_file.display(), err))?;
    let mut decoder = GzDecoder::new(&compressed[..]);
    let mut content = String::new();
    decoder
        .read_to_string(&mut content)
        .map_err(|err| format!("failed to decode {}: {}", synctex_file.display(), err))?;
    Ok(content)
}

fn synctex_page_open(line: &str) -> Option<usize> {
    line.strip_prefix('{')?.parse::<usize>().ok()
}

fn synctex_page_close(line: &str) -> Option<usize> {
    line.strip_prefix('}')?.parse::<usize>().ok()
}

fn parse_synctex_record_count(lines: &[String], postamble_idx: usize) -> usize {
    lines[postamble_idx + 1..]
        .iter()
        .find_map(|line| line.strip_prefix("Count:")?.parse::<usize>().ok())
        .unwrap_or(0)
}

fn parse_synctex_document(content: &str) -> std::result::Result<ParsedSyncTex, String> {
    let lines: Vec<String> = content.lines().map(str::to_owned).collect();
    let content_idx = lines
        .iter()
        .position(|line| line == "Content:")
        .ok_or_else(|| String::from("SyncTeX file has no Content section"))?;
    let postamble_idx = lines
        .iter()
        .position(|line| line == "Postamble:")
        .ok_or_else(|| String::from("SyncTeX file has no Postamble section"))?;

    if postamble_idx <= content_idx {
        return Err(String::from("SyncTeX Postamble appears before Content"));
    }

    let header_lines = lines[..content_idx].to_vec();
    let input_lines: Vec<(u32, String)> = lines[..postamble_idx]
        .iter()
        .filter_map(|line| parse_synctex_input_line(line).map(|(tag, path)| (tag, path.to_owned())))
        .collect();

    if input_lines.is_empty() {
        return Err(String::from("SyncTeX file has no Input lines"));
    }

    Ok(ParsedSyncTex {
        header_lines,
        input_lines,
        content_lines: lines[content_idx + 1..postamble_idx].to_vec(),
        record_count: parse_synctex_record_count(&lines, postamble_idx),
    })
}

fn remap_synctex_global_tag(
    line: &str,
    tag_map: &HashMap<u32, u32>,
) -> std::result::Result<String, String> {
    let first_char = match line.chars().next() {
        Some(ch) if matches!(ch, '[' | '(' | 'x' | 'k' | 'g' | '$' | 'v' | 'h' | 'r') => ch,
        _ => return Ok(line.to_owned()),
    };

    let prefix_len = first_char.len_utf8();
    let rest = &line[prefix_len..];
    let colon_idx = match rest.find(':') {
        Some(idx) => idx,
        None => return Ok(line.to_owned()),
    };
    let link = &rest[..colon_idx];
    let mut parts = link.split(',');
    let tag = match parts.next().and_then(|part| part.parse::<u32>().ok()) {
        Some(tag) => tag,
        None => return Ok(line.to_owned()),
    };
    let global_tag = tag_map
        .get(&tag)
        .ok_or_else(|| format!("SyncTeX record references unknown input tag {}", tag))?;

    let mut rewritten_link = global_tag.to_string();
    for part in parts {
        rewritten_link.push(',');
        rewritten_link.push_str(part);
    }

    Ok(format!(
        "{}{}{}",
        &line[..prefix_len],
        rewritten_link,
        &rest[colon_idx..]
    ))
}

fn rewrite_synctex_content_lines(
    content_lines: &[String],
    page_offset: usize,
    tag_map: &HashMap<u32, u32>,
) -> std::result::Result<(Vec<String>, usize), String> {
    let mut rewritten = Vec::with_capacity(content_lines.len());
    let mut local_page_count = 0usize;

    for line in content_lines {
        if parse_synctex_input_line(line).is_some() {
            continue;
        }

        if let Some(page_number) = synctex_page_open(line) {
            if page_number == 0 {
                rewritten.push(line.clone());
            } else {
                local_page_count = local_page_count.max(page_number);
                rewritten.push(format!("{{{}", page_offset + page_number));
            }
        } else if let Some(page_number) = synctex_page_close(line) {
            if page_number == 0 {
                rewritten.push(line.clone());
            } else {
                local_page_count = local_page_count.max(page_number);
                rewritten.push(format!("}}{}", page_offset + page_number));
            }
        } else {
            rewritten.push(remap_synctex_global_tag(line, tag_map)?);
        }
    }

    if local_page_count == 0 {
        Err(String::from("SyncTeX content has no page sheet records"))
    } else {
        Ok((rewritten, local_page_count))
    }
}

fn append_merged_synctex_header(
    merged_lines: &mut Vec<String>,
    header_lines: &[String],
    input_lines: &[(u32, String)],
    output_file: &str,
) {
    let output_line = format!("Output:{}", synctex_path(Path::new(output_file)));
    let mut inserted_inputs = false;
    let mut wrote_output = false;

    for line in header_lines {
        if parse_synctex_input_line(line).is_some() {
            if !inserted_inputs {
                for (tag, path) in input_lines {
                    merged_lines.push(format!("Input:{}:{}", tag, path));
                }
                inserted_inputs = true;
            }
            continue;
        }

        if line.starts_with("Output:") {
            if !inserted_inputs {
                for (tag, path) in input_lines {
                    merged_lines.push(format!("Input:{}:{}", tag, path));
                }
                inserted_inputs = true;
            }
            merged_lines.push(output_line.clone());
            wrote_output = true;
        } else {
            merged_lines.push(line.clone());
        }
    }

    if !inserted_inputs {
        for (tag, path) in input_lines {
            merged_lines.push(format!("Input:{}:{}", tag, path));
        }
    }

    if !wrote_output {
        merged_lines.push(output_line);
    }
}

fn build_merged_frame_synctex(
    generated_documents: &[GeneratedDocument],
    cache_subdir: &Path,
    output_file: &str,
) -> std::result::Result<(String, usize), String> {
    let mut first_header_lines = None;
    let mut global_tag_by_path = HashMap::new();
    let mut global_input_lines = Vec::new();
    let mut merged_page_lines = Vec::new();
    let mut page_offset = 0usize;
    let mut record_count = 0usize;

    for document in generated_documents {
        let frame_pdf = compiled_pdf_path(cache_subdir, &document.sync_map.temp_file_name);
        let synctex_file = frame_pdf.with_extension("synctex.gz");
        let content = read_synctex_contents(&synctex_file)?;
        let remapped_content = remap_synctex_contents(&content, &document.sync_map);
        let parsed = parse_synctex_document(&remapped_content)?;
        let mut local_tag_map = HashMap::new();

        if first_header_lines.is_none() {
            first_header_lines = Some(parsed.header_lines.clone());
        }
        record_count += parsed.record_count;

        for (local_tag, path) in parsed.input_lines {
            let global_tag = match global_tag_by_path.get(&path) {
                Some(tag) => *tag,
                None => {
                    let tag = (global_input_lines.len() + 1) as u32;
                    global_tag_by_path.insert(path.clone(), tag);
                    global_input_lines.push((tag, path));
                    tag
                }
            };
            local_tag_map.insert(local_tag, global_tag);
        }

        let (rewritten, local_page_count) =
            rewrite_synctex_content_lines(&parsed.content_lines, page_offset, &local_tag_map)?;
        merged_page_lines.extend(rewritten);
        page_offset += local_page_count;
    }

    let header_lines =
        first_header_lines.ok_or_else(|| String::from("there are no frame SyncTeX files"))?;
    let page_count = page_offset;
    let mut merged_lines = Vec::new();
    append_merged_synctex_header(
        &mut merged_lines,
        &header_lines,
        &global_input_lines,
        output_file,
    );
    merged_lines.push(String::from("Content:"));
    merged_lines.extend(merged_page_lines);
    merged_lines.push(String::from("Postamble:"));
    merged_lines.push(format!("Count:{}", record_count));
    merged_lines.push(String::from("!0"));
    merged_lines.push(String::from("Post scriptum:"));

    let mut merged = merged_lines.join("\n");
    merged.push('\n');
    Ok((merged, page_count))
}

fn publish_synctex_contents(content: &str, output_file: &str) -> Result<()> {
    let output_synctex = Path::new(output_file).with_extension("synctex.gz");
    if let Some(parent) = output_synctex.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| {
                error!(
                    "Failed to create SyncTeX output directory {}: {}",
                    parent.display(),
                    err
                );
                FasterBeamerError::IoError
            })?;
        }
    }

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(content.as_bytes()).map_err(|err| {
        error!(
            "Failed to encode SyncTeX for {}: {}",
            output_synctex.display(),
            err
        );
        FasterBeamerError::IoError
    })?;
    let compressed = encoder.finish().map_err(|err| {
        error!(
            "Failed to finish SyncTeX encoding for {}: {}",
            output_synctex.display(),
            err
        );
        FasterBeamerError::IoError
    })?;

    fs::write(&output_synctex, compressed).map_err(|err| {
        error!(
            "Failed to write SyncTeX file {}: {}",
            output_synctex.display(),
            err
        );
        FasterBeamerError::IoError
    })?;

    info!("Published SyncTeX: {}", output_synctex.display());
    Ok(())
}

fn rewrite_synctex_to_original(compiled_pdf: &Path, sync_map: &FrameSyncTexMap) -> Result<()> {
    let synctex_file = compiled_pdf.with_extension("synctex.gz");
    if !synctex_file.is_file() {
        return Ok(());
    }

    let compressed = std::fs::read(&synctex_file).map_err(|err| {
        error!(
            "Failed to read SyncTeX file {}: {}",
            synctex_file.display(),
            err
        );
        FasterBeamerError::IoError
    })?;

    let mut decoder = GzDecoder::new(&compressed[..]);
    let mut content = String::new();
    decoder.read_to_string(&mut content).map_err(|err| {
        error!(
            "Failed to decode SyncTeX file {}: {}",
            synctex_file.display(),
            err
        );
        FasterBeamerError::IoError
    })?;

    let rewritten = remap_synctex_contents(&content, sync_map);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(rewritten.as_bytes()).map_err(|err| {
        error!(
            "Failed to encode SyncTeX file {}: {}",
            synctex_file.display(),
            err
        );
        FasterBeamerError::IoError
    })?;
    let compressed = encoder.finish().map_err(|err| {
        error!(
            "Failed to finish SyncTeX encoding for {}: {}",
            synctex_file.display(),
            err
        );
        FasterBeamerError::IoError
    })?;

    std::fs::write(&synctex_file, compressed).map_err(|err| {
        error!(
            "Failed to write SyncTeX file {}: {}",
            synctex_file.display(),
            err
        );
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
                rewritten_lines.push(format!(
                    "Input:{}:{}",
                    tag,
                    synctex_path(&sync_map.source_file)
                ));
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

fn remap_synctex_link_line(
    line: &str,
    temp_tag: u32,
    sync_map: &FrameSyncTexMap,
) -> Option<String> {
    let first_char = line.chars().next()?;
    if !matches!(
        first_char,
        '[' | '(' | 'x' | 'k' | 'g' | '$' | 'v' | 'h' | 'r'
    ) {
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
    text[..byte_idx]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
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

fn strip_tex_comment_from_line(line: &str) -> &str {
    let mut escaped = false;

    for (idx, ch) in line.char_indices() {
        if ch == '%' && !escaped {
            return &line[..idx];
        }

        if ch == '\\' {
            escaped = !escaped;
        } else {
            escaped = false;
        }
    }

    line
}

fn brace_delta(line: &str) -> isize {
    let mut escaped = false;
    let mut delta = 0isize;

    for ch in line.chars() {
        match ch {
            '\\' => escaped = !escaped,
            '{' if !escaped => {
                delta += 1;
                escaped = false;
            }
            '}' if !escaped => {
                delta -= 1;
                escaped = false;
            }
            _ => escaped = false,
        }
    }

    delta
}

fn command_name_at_line_start(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let command = trimmed.strip_prefix('\\')?;
    let end = command
        .char_indices()
        .find(|(_, ch)| !ch.is_ascii_alphabetic())
        .map(|(idx, _)| idx)
        .unwrap_or(command.len());

    if end == 0 {
        None
    } else {
        Some(&command[..end])
    }
}

fn is_document_context_command(command: &str) -> bool {
    matches!(
        command,
        "def"
            | "gdef"
            | "edef"
            | "xdef"
            | "let"
            | "newcommand"
            | "renewcommand"
            | "providecommand"
            | "DeclareRobustCommand"
            | "NewDocumentCommand"
            | "RenewDocumentCommand"
            | "ProvideDocumentCommand"
            | "DeclareDocumentCommand"
            | "newenvironment"
            | "renewenvironment"
            | "NewDocumentEnvironment"
            | "RenewDocumentEnvironment"
            | "ProvideDocumentEnvironment"
            | "DeclareDocumentEnvironment"
            | "newtheorem"
            | "theoremstyle"
            | "newlength"
            | "setlength"
            | "addtolength"
            | "settowidth"
            | "settoheight"
            | "settodepth"
            | "newcounter"
            | "setcounter"
            | "addtocounter"
            | "counterwithin"
            | "counterwithout"
            | "definecolor"
            | "colorlet"
            | "tikzset"
            | "pgfplotsset"
            | "hypersetup"
            | "lstset"
            | "setminted"
            | "setbeamertemplate"
            | "setbeamercolor"
            | "setbeamerfont"
            | "makeatletter"
            | "makeatother"
            | "ExplSyntaxOn"
            | "ExplSyntaxOff"
    ) || command.starts_with("Declare")
        || command.starts_with("New")
        || command.starts_with("Renew")
        || command.starts_with("Provide")
}

fn is_title_info_command(command: &str) -> bool {
    matches!(
        command,
        "title" | "subtitle" | "author" | "date" | "institute"
    )
}

fn extract_title_commands(preamble: &mut String) -> String {
    let mut cleaned_preamble = String::with_capacity(preamble.len());
    let mut title_info = String::new();
    let mut collecting = false;
    let mut balance = 0isize;

    for line in preamble.split_inclusive('\n') {
        let uncommented = strip_tex_comment_from_line(line);

        if collecting {
            title_info.push_str(line);
            balance += brace_delta(uncommented);
            if balance <= 0 {
                collecting = false;
                balance = 0;
            }
        } else if command_name_at_line_start(uncommented)
            .map(is_title_info_command)
            .unwrap_or(false)
        {
            title_info.push_str(line);
            balance = brace_delta(uncommented);
            if balance <= 0 {
                balance = 0;
            } else {
                collecting = true;
            }
        } else {
            cleaned_preamble.push_str(line);
        }
    }

    *preamble = cleaned_preamble;
    title_info
}

fn extract_document_context_snippets(
    source_content: &str,
    segment_start_idx: usize,
    segment: &str,
) -> Vec<DocumentContextSnippet> {
    let mut snippets = Vec::new();
    let mut collecting = false;
    let mut collected = String::new();
    let mut collected_start_idx = 0usize;
    let mut balance = 0isize;
    let mut segment_offset = 0usize;

    for line in segment.split_inclusive('\n') {
        let uncommented = strip_tex_comment_from_line(line);

        if collecting {
            collected.push_str(line);
            balance += brace_delta(uncommented);

            if balance <= 0 {
                snippets.push(DocumentContextSnippet {
                    content: collected.clone(),
                    source_start_line: line_number_at(source_content, collected_start_idx),
                });
                collected.clear();
                collecting = false;
                balance = 0;
            }
        } else if command_name_at_line_start(uncommented)
            .map(is_document_context_command)
            .unwrap_or(false)
        {
            collected_start_idx = segment_start_idx + segment_offset;
            collected.push_str(line);
            balance = brace_delta(uncommented);

            if balance <= 0 {
                snippets.push(DocumentContextSnippet {
                    content: collected.clone(),
                    source_start_line: line_number_at(source_content, collected_start_idx),
                });
                collected.clear();
                balance = 0;
            } else {
                collecting = true;
            }
        }

        segment_offset += line.len();
    }

    if collecting && !collected.is_empty() {
        snippets.push(DocumentContextSnippet {
            content: collected,
            source_start_line: line_number_at(source_content, collected_start_idx),
        });
    }

    snippets
}

fn document_contexts_before_frames(
    source_content: &str,
    frame_ranges: &[(usize, usize)],
    document_begin_idx: Option<usize>,
) -> Vec<Vec<DocumentContextSnippet>> {
    let document_start_idx = document_begin_idx
        .map(|idx| idx + "\\begin{document}".len())
        .unwrap_or(0);
    let mut contexts = Vec::with_capacity(frame_ranges.len());
    let mut accumulated = Vec::new();
    let mut source_cursor = document_start_idx;

    for (frame_start_idx, frame_end_idx) in frame_ranges {
        if *frame_start_idx >= document_start_idx {
            if source_cursor < *frame_start_idx {
                accumulated.extend(extract_document_context_snippets(
                    source_content,
                    source_cursor,
                    &source_content[source_cursor..*frame_start_idx],
                ));
            }
            contexts.push(accumulated.clone());
            source_cursor = *frame_end_idx;
        } else {
            contexts.push(Vec::new());
        }
    }

    contexts
}

fn append_document_context(
    compile_prefix: &mut String,
    segments: &mut Vec<SyncTexLineSegment>,
    context: &[DocumentContextSnippet],
) {
    for snippet in context {
        let temp_start_line = logical_line_count(compile_prefix) + 1;
        compile_prefix.push_str(&snippet.content);
        if !snippet.content.ends_with('\n') {
            compile_prefix.push('\n');
        }

        let line_count = logical_line_count(&snippet.content);
        if line_count > 0 {
            segments.push(SyncTexLineSegment {
                temp_start_line,
                line_count,
                source_start_line: snippet.source_start_line,
            });
        }
    }
}

fn frame_contains_table_of_contents(frame: &str) -> bool {
    TOC_REGEX.is_match(&strip_tex_comments(frame))
}

fn frame_contains_title_page(frame: &str) -> bool {
    TITLE_PAGE_REGEX.is_match(&strip_tex_comments(frame))
}

fn frame_labels(frames: &[String]) -> Vec<FrameLabel> {
    let mut next_number = 1usize;
    let mut front_matter_open = true;
    let mut toc_seen = false;

    frames
        .iter()
        .map(|frame| {
            if front_matter_open && frame_contains_title_page(frame) {
                FrameLabel::Title
            } else if front_matter_open && !toc_seen && frame_contains_table_of_contents(frame) {
                toc_seen = true;
                front_matter_open = false;
                FrameLabel::Toc
            } else {
                front_matter_open = false;
                let label = FrameLabel::Number(next_number);
                next_number += 1;
                label
            }
        })
        .collect()
}

fn frame_matches_filter(frame_label: &FrameLabel, frame_content: &str, filters: &[String]) -> bool {
    for filter in filters {
        if filter.eq_ignore_ascii_case("title") {
            if matches!(frame_label, FrameLabel::Title) {
                return true;
            }
            continue;
        }
        if filter.eq_ignore_ascii_case("toc") {
            if matches!(frame_label, FrameLabel::Toc) {
                return true;
            }
            continue;
        }
        if let Ok(num) = filter.parse::<usize>() {
            if matches!(frame_label, FrameLabel::Number(n) if *n == num) {
                return true;
            }
            continue;
        }
        if frame_content.contains(filter) {
            return true;
        }
    }
    false
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

    let source_toc_path =
        input_dir.join(Path::new(tex_input_name(input_path)).with_extension("toc"));
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

fn collect_graphics_paths(tex: &str, base_dir: &Path, inherited_paths: &[PathBuf]) -> Vec<PathBuf> {
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
        "includegraphics" | "includepdf" => resolve_graphics_from_paths(raw_path, graphics_paths),
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

        let Some(resolved) = resolve_related_file(command, raw_path, base_dir, &graphics_paths)
        else {
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
        error!(
            "Failed to read recorder file {}: {}",
            fls_path.display(),
            err
        );
        FasterBeamerError::IoError
    })?;
    let dependencies = parse_fls_dependencies(&content, cache_subdir);
    write_dependency_manifest(cache_subdir, temp_file_name, &dependencies)
}

fn dependencies_for_document(cache_subdir: &Path, document: &GeneratedDocument) -> Vec<PathBuf> {
    read_dependency_manifest(cache_subdir, &document.sync_map.temp_file_name)
        .unwrap_or_else(|| document.dependencies.clone())
}

fn validate_compiled_pdf(compiled_pdf: &Path) -> std::io::Result<()> {
    let content = fs::read(compiled_pdf)?;
    if !content.starts_with(b"%PDF-") {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "PDF header is missing",
        ));
    }

    let tail_start = content.len().saturating_sub(2048);
    if !content[tail_start..]
        .windows(b"%%EOF".len())
        .any(|window| window == b"%%EOF")
    {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "PDF trailer is missing",
        ));
    }

    Ok(())
}

fn compiled_output_is_fresh(compiled_pdf: &Path, dependencies: &[PathBuf]) -> bool {
    if validate_compiled_pdf(compiled_pdf).is_err() {
        return false;
    }

    let compiled_modified =
        match std::fs::metadata(compiled_pdf).and_then(|metadata| metadata.modified()) {
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
    if args.is_present("pdfunite-synctex") {
        "pdfunite-synctex"
    } else if args.is_present("pdfunite") {
        "pdfunite"
    } else if args.is_present("tex-unite") {
        "tex-unite"
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

fn first_changed_frame_label(
    first_changed_frame: usize,
    frame_labels: &[FrameLabel],
    numbered_frame_count: usize,
) -> String {
    if frame_labels.is_empty() || first_changed_frame >= frame_labels.len() {
        String::from("none")
    } else {
        frame_label_for_index(frame_labels, first_changed_frame, numbered_frame_count)
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
        let path_offset = united_tex[search_cursor..]
            .find(&frame_pdf)
            .ok_or_else(|| {
                error!("Failed to locate included PDF path while building united SyncTeX mapping.");
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

fn unix_timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn compile_united_artifacts(
    source_content: &str,
    frames: &[String],
    frame_source_lines: &[(usize, usize)],
    generated_documents: &[GeneratedDocument],
    cache_subdir: &Path,
    original_source_path: &Path,
    input_dir: &Path,
    latex_engine: LatexEngine,
    compiler_options: &[String],
    run_options: LatexRunOptions,
) -> Result<UnitedCompileArtifacts> {
    let (united_tex, mut united_sync_map) = build_united_document(
        source_content,
        frames,
        frame_source_lines,
        generated_documents,
        cache_subdir,
        original_source_path,
    )?;

    let united_job_name = format!("{}{}", UNITED_TEMP_PREFIX, unix_timestamp_millis());
    united_sync_map.temp_file_name = format!("{}.tex", united_job_name);

    let united_tex_file = input_dir.join(&united_sync_map.temp_file_name);
    let united_pdf = cache_subdir.join(format!("{}.pdf", united_job_name));
    write(&united_tex_file, united_tex).map_err(|err| {
        error!("Failed to write united.tex: {}", err);
        FasterBeamerError::IoError
    })?;

    let compiler = apply_compiler_options(
        LatexCompiler::new_in_with_engine(cache_subdir.to_path_buf(), latex_engine)
            .add_arg("-shell-escape")
            .add_arg("-interaction=nonstopmode")
            .with_current_dir(input_dir.to_path_buf()),
        compiler_options,
    );

    let compile_result = compiler.run(
        Path::new(tex_input_name(&united_tex_file)),
        &LatexInput::new(),
        run_options,
    );

    if let Err(err) = compile_result {
        let error_message = format!("{}", err);
        if let Err(_err) = write_master_log_for_united_failure(
            original_source_path,
            cache_subdir,
            &united_sync_map,
            &error_message,
        ) {
            warn!(
                "Failed to write source log for united compilation failure: {}",
                original_source_path.display()
            );
        }
        error!(
            "Failed to run united TeX compile. Details were written to {}.",
            original_source_path.with_extension("log").display()
        );
        error!("Reason: {}", error_message);
    }

    if united_pdf.is_file() {
        Ok(UnitedCompileArtifacts {
            tex_file: united_tex_file,
            pdf_file: united_pdf,
            sync_map: united_sync_map,
        })
    } else {
        error!("Compilation failed!");
        Err(FasterBeamerError::CompileError)
    }
}

fn tex_input_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .expect("Expected a TeX input file name")
}

fn default_output_file(input_path: &Path) -> String {
    input_path
        .with_extension("pdf")
        .to_string_lossy()
        .into_owned()
}

fn has_tex_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case("tex"))
        .unwrap_or(false)
}

pub fn resolve_input_file(input_file: &str) -> PathBuf {
    let input_path = PathBuf::from(input_file);
    if input_path.is_file() || has_tex_extension(&input_path) {
        input_path
    } else {
        PathBuf::from(format!("{}.tex", input_file))
    }
}

fn frame_temp_file_name(hash: &md5::Digest) -> String {
    format!("{}{:x}.tex", FRAME_TEMP_PREFIX, hash)
}

fn append_latex_cache_key(
    input: &mut String,
    latex_engine: LatexEngine,
    precompile_preamble: bool,
    compiler_options: &[String],
) {
    input.push_str("\n% faster-beamer latex engine\n");
    input.push_str(latex_engine.command_name());
    input.push('\n');
    input.push_str(if precompile_preamble {
        "% faster-beamer precompile preamble: true\n"
    } else {
        "% faster-beamer precompile preamble: false\n"
    });

    if compiler_options.is_empty() {
        return;
    }

    input.push_str("% faster-beamer compiler options\n");
    for option in compiler_options {
        input.push_str(option);
        input.push('\n');
    }
}

fn preamble_job_name(preamble_hash: &md5::Digest) -> String {
    format!("{}{:x}", PREAMBLE_TEMP_PREFIX, preamble_hash)
}

fn compiled_pdf_path(cache_subdir: &Path, temp_file_name: &str) -> PathBuf {
    cache_subdir.join(Path::new(temp_file_name).with_extension("pdf"))
}

fn remove_file_if_exists_with_retries(path: &Path) -> std::io::Result<()> {
    const REMOVE_RETRY_DELAYS_MS: [u64; 8] = [25, 50, 75, 100, 150, 200, 300, 500];

    for delay_ms in REMOVE_RETRY_DELAYS_MS {
        match fs::remove_file(path) {
            Ok(_) => return Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(err) if is_transient_file_lock(&err) => {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
            Err(err) => return Err(err),
        }
    }

    match fs::remove_file(path) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn is_transient_file_lock(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(32) || matches!(err.kind(), ErrorKind::PermissionDenied)
}

fn remove_frame_job_sidecars(cache_subdir: &Path, temp_file_name: &str) -> std::io::Result<()> {
    let mut first_error = None;

    for extension in FRAME_JOB_SIDECAR_EXTENSIONS {
        let path = cache_subdir.join(Path::new(temp_file_name).with_extension(extension));
        if let Err(err) = remove_file_if_exists_with_retries(&path) {
            first_error.get_or_insert(err);
        }
    }

    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn dependency_sidecar_path(
    cache_subdir: &Path,
    input_dir: &Path,
    dependency: &Path,
    extension: &str,
) -> PathBuf {
    let cache_relative_path = dependency
        .strip_prefix(input_dir)
        .ok()
        .or_else(|| dependency.file_name().map(Path::new))
        .unwrap_or(dependency);

    cache_subdir
        .join(cache_relative_path)
        .with_extension(extension)
}

fn remove_dependency_job_sidecars(
    cache_subdir: &Path,
    input_dir: &Path,
    dependencies: &[PathBuf],
) -> std::io::Result<()> {
    let mut first_error = None;

    for dependency in dependencies
        .iter()
        .filter(|dependency| has_tex_extension(dependency))
    {
        for extension in FRAME_JOB_SIDECAR_EXTENSIONS {
            let path = dependency_sidecar_path(cache_subdir, input_dir, dependency, extension);
            if let Err(err) = remove_file_if_exists_with_retries(&path) {
                first_error.get_or_insert(err);
            }
        }
    }

    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn current_cache_paths(input_path: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let cwd = current_dir().unwrap();
    let input_dir = input_path
        .parent()
        .unwrap_or(&cwd)
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_owned());
    let cachedir = configured_cache_dir().expect("This OS is not supported");
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
        .map(|content| {
            content.contains("\\addtocounter{framenumber}") && content.contains("\\end{document}")
        })
        .unwrap_or(false)
}

fn is_legacy_preamble_temp_file(file_name: &str) -> bool {
    for extension in [".fmt", ".log"] {
        if let Some(stem) = file_name.strip_suffix(extension) {
            let mut parts = stem.rsplitn(2, '_');
            let draft_flag = parts.next();
            let digest = parts.next();
            if matches!(draft_flag, Some("true") | Some("false"))
                && digest.map(is_hex_digest).unwrap_or(false)
            {
                return true;
            }
        }
    }

    false
}

fn clean_prefixed_files(input_dir: &Path) -> Result<usize> {
    let mut removed = 0;
    let entries = std::fs::read_dir(input_dir).map_err(|err| {
        error!(
            "Failed to read input directory {}: {}",
            input_dir.display(),
            err
        );
        FasterBeamerError::IoError
    })?;

    for entry in entries {
        let entry = entry.map_err(|err| {
            error!(
                "Failed to inspect input directory {}: {}",
                input_dir.display(),
                err
            );
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
                error!(
                    "Failed to remove temporary file {}: {}",
                    path.display(),
                    err
                );
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
                warn!(
                    "Failed to prune empty cache directory {}: {}",
                    dir.display(),
                    err
                );
                break;
            }
        }
    }
}

fn cache_garbage_cutoff() -> SystemTime {
    SystemTime::now() - Duration::from_secs(CACHE_GARBAGE_RETENTION_DAYS * 24 * 60 * 60)
}

fn cache_sweep_is_due(cache_dir: &Path) -> bool {
    let stamp_path = cache_dir.join(CACHE_GARBAGE_SWEEP_STAMP);
    let interval = Duration::from_secs(CACHE_GARBAGE_SWEEP_INTERVAL_HOURS * 60 * 60);

    match fs::metadata(&stamp_path).and_then(|metadata| metadata.modified()) {
        Ok(modified) => modified
            .elapsed()
            .map(|elapsed| elapsed >= interval)
            .unwrap_or(true),
        Err(err) if err.kind() == ErrorKind::NotFound => true,
        Err(err) => {
            warn!(
                "Failed to inspect cache cleanup stamp {}: {}",
                stamp_path.display(),
                err
            );
            true
        }
    }
}

fn mark_cache_sweep(cache_dir: &Path) {
    let stamp_path = cache_dir.join(CACHE_GARBAGE_SWEEP_STAMP);
    if let Err(err) = fs::write(&stamp_path, b"") {
        warn!(
            "Failed to update cache cleanup stamp {}: {}",
            stamp_path.display(),
            err
        );
    }
}

fn path_is_inside(path: &Path, parent: &Path) -> bool {
    path == parent || path.starts_with(parent)
}

fn cache_entry_is_stale(path: &Path, cutoff: SystemTime) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map(|modified| modified < cutoff)
        .unwrap_or(false)
}

fn remove_stale_cache_entries(
    dir: &Path,
    active_cache_subdir: &Path,
    cutoff: SystemTime,
) -> std::io::Result<usize> {
    let mut removed = 0;

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        if path_is_inside(&path, active_cache_subdir) {
            continue;
        }

        if file_type.is_dir() {
            removed += remove_stale_cache_entries(&path, active_cache_subdir, cutoff)?;
            match fs::remove_dir(&path) {
                Ok(_) => removed += 1,
                Err(err) if err.kind() == ErrorKind::DirectoryNotEmpty => {}
                Err(err) if err.kind() == ErrorKind::NotFound => {}
                Err(err) => {
                    warn!(
                        "Failed to prune cache directory {}: {}",
                        path.display(),
                        err
                    );
                }
            }
        } else if file_type.is_file() && cache_entry_is_stale(&path, cutoff) {
            match remove_file_if_exists_with_retries(&path) {
                Ok(_) => removed += 1,
                Err(err) => warn!(
                    "Failed to remove stale cache file {}: {}",
                    path.display(),
                    err
                ),
            }
        }
    }

    Ok(removed)
}

fn remove_cache_garbage(cache_dir: &Path, active_cache_subdir: &Path) {
    if !cache_dir.is_dir() || !cache_sweep_is_due(cache_dir) {
        return;
    }

    match remove_stale_cache_entries(cache_dir, active_cache_subdir, cache_garbage_cutoff()) {
        Ok(removed) => {
            if removed > 0 {
                info!(
                    "Cache cleanup: removed {} stale entries from {}.",
                    removed,
                    cache_dir.display()
                );
            }
            mark_cache_sweep(cache_dir);
        }
        Err(err) => warn!("Cache cleanup failed for {}: {}", cache_dir.display(), err),
    }
}

pub fn clean_generated_artifacts(input_file: &str, args: &ArgMatches) -> Result<()> {
    let input_path = resolve_input_file(input_file);
    if !input_path.is_file() {
        error!("Could not open {}", input_path.display());
        return Err(FasterBeamerError::InputFileNotExistent);
    }

    let (input_dir, cachedir, cache_subdir) = current_cache_paths(&input_path);
    let output_file = output_file_arg(args, &input_path);
    let removed_input_files = clean_prefixed_files(&input_dir)?;

    if cache_subdir.is_dir() {
        std::fs::remove_dir_all(&cache_subdir).map_err(|err| {
            error!(
                "Failed to remove cache directory {}: {}",
                cache_subdir.display(),
                err
            );
            FasterBeamerError::IoError
        })?;
        prune_empty_cache_dirs(&cachedir, &cache_subdir);
    }

    clear_published_synctex(&output_file);
    info!(
        "Removed faster-beamer artifacts for {} ({} stale source temp files).",
        input_file, removed_input_files
    );

    Ok(())
}

pub fn clean_all_generated_artifacts() -> Result<()> {
    let cachedir = configured_cache_dir().expect("This OS is not supported");
    if cachedir
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        error!(
            "Refusing to clean a cache path containing a parent component: {}",
            cachedir.display()
        );
        return Err(FasterBeamerError::IoError);
    }
    if cachedir.parent().is_none() {
        error!(
            "Refusing to remove a filesystem root configured as the cache: {}",
            cachedir.display()
        );
        return Err(FasterBeamerError::IoError);
    }
    match crate::guard::has_live_guards() {
        Ok(true) => {
            error!("Refusing to clean all caches while a faster-beamer watcher is active.");
            return Err(FasterBeamerError::IoError);
        }
        Ok(false) => {}
        Err(err) => {
            error!("Failed to inspect watcher guards: {}", err);
            return Err(FasterBeamerError::IoError);
        }
    }

    match std::fs::symlink_metadata(&cachedir) {
        Ok(metadata) if metadata.is_dir() => {
            let resolved_cachedir = std::fs::canonicalize(&cachedir).map_err(|err| {
                error!(
                    "Failed to resolve cache directory {}: {}",
                    cachedir.display(),
                    err
                );
                FasterBeamerError::IoError
            })?;
            if resolved_cachedir.parent().is_none() {
                error!(
                    "Refusing to remove a filesystem root configured as the cache: {}",
                    cachedir.display()
                );
                return Err(FasterBeamerError::IoError);
            }

            std::fs::remove_dir_all(&resolved_cachedir).map_err(|err| {
                error!(
                    "Failed to remove cache directory {}: {}",
                    cachedir.display(),
                    err
                );
                FasterBeamerError::IoError
            })?;
            info!(
                "Removed all faster-beamer cached artifacts from {}.",
                cachedir.display()
            );
        }
        Ok(_) => {
            error!(
                "Configured cache path is not a directory: {}",
                cachedir.display()
            );
            return Err(FasterBeamerError::IoError);
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            info!("No faster-beamer cache found at {}.", cachedir.display());
        }
        Err(err) => {
            error!(
                "Failed to inspect cache directory {}: {}",
                cachedir.display(),
                err
            );
            return Err(FasterBeamerError::IoError);
        }
    }

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

fn latex_engine(args: &ArgMatches) -> LatexEngine {
    args.value_of("engine")
        .and_then(LatexEngine::from_name)
        .unwrap_or(LatexEngine::PdfLatex)
}

fn precompile_preamble(args: &ArgMatches, latex_engine: LatexEngine) -> bool {
    if args.is_present("precompile-preamble") {
        true
    } else if args.is_present("no-precompile-preamble") {
        false
    } else {
        latex_engine.precompiles_preamble_by_default()
    }
}

fn parallel_job_count(args: &ArgMatches) -> Option<usize> {
    args.value_of("jobs")
        .and_then(|count| count.parse::<usize>().ok())
}

fn effective_parallel_job_count(
    latex_engine: LatexEngine,
    parallel_requested: bool,
    explicit_job_count: Option<usize>,
) -> Option<usize> {
    explicit_job_count.or_else(|| {
        if parallel_requested && latex_engine == LatexEngine::LuaLatex {
            Some(LUALATEX_AUTO_PARALLEL_JOBS)
        } else {
            None
        }
    })
}

fn apply_compiler_options(
    mut compiler: LatexCompiler,
    compiler_options: &[String],
) -> LatexCompiler {
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

fn latex_run_options(
    latex_pass_count: usize,
    bibliography: Option<BibliographyTool>,
) -> LatexRunOptions {
    LatexRunOptions::new()
        .with_latex_pass_count(latex_pass_count)
        .with_bibliography_tool(bibliography)
}

pub fn process_file(input_file: &str, args: &ArgMatches) -> Result<()> {
    let total_start_time = std::time::Instant::now();
    let mut step_start_time = total_start_time;
    let input_path = resolve_input_file(input_file);
    let (input_dir, cachedir, cache_subdir) = current_cache_paths(&input_path);
    let original_source_path = input_path
        .canonicalize()
        .unwrap_or_else(|_| input_dir.join(tex_input_name(&input_path)));
    let output_file = output_file_arg(args, &input_path);
    let correct_frame_numbers = args.is_present("frame-numbers");
    let latex_pass_count = latex_pass_count(args);
    let bibliography = bibliography_tool(args);
    let run_options = latex_run_options(latex_pass_count, bibliography);
    let force_recompile = args.is_present("force-recompile");
    let compiler_options = compiler_options(args);
    let selected_engine = latex_engine(args);
    let precompile_preamble = precompile_preamble(args, selected_engine);
    let explicit_parallel_job_count = parallel_job_count(args);
    let parallel_requested = args.is_present("parallel");
    let lualatex_auto_parallel_capped = parallel_requested
        && explicit_parallel_job_count.is_none()
        && selected_engine == LatexEngine::LuaLatex;
    let parallel_job_count = effective_parallel_job_count(
        selected_engine,
        parallel_requested,
        explicit_parallel_job_count,
    );
    let use_parallel = args.is_present("parallel") || parallel_job_count.is_some();
    let build_mode = build_mode_label(args);

    if !input_path.is_file() {
        error!("Could not open {}", input_path.display());
        return Err(FasterBeamerError::InputFileNotExistent);
    }
    if lualatex_auto_parallel_capped {
        warn!(
            "Parallel: lualatex auto parallelism is capped at {} jobs; pass --jobs COUNT to override.",
            LUALATEX_AUTO_PARALLEL_JOBS
        );
    }
    remove_cache_garbage(&cachedir, &cache_subdir);

    let parsed_file = parsing::ParsedFile::new(input_path.to_string_lossy().into_owned());
    trace!("{}", parsed_file.syntax_tree.root_node().to_sexp());

    let frame_nodes = if args.is_present("tree-sitter") {
        get_frames(&parsed_file)
    } else {
        Vec::new()
    };

    let mut frames = Vec::with_capacity(frame_nodes.len());
    let mut frame_source_lines = Vec::with_capacity(frame_nodes.len());
    let mut frame_source_ranges = Vec::with_capacity(frame_nodes.len());
    if !frame_nodes.is_empty() {
        for f in frame_nodes.iter() {
            let node_string = parsed_file.get_node_string(&f);
            frames.push(node_string.to_string());
            frame_source_lines.push((
                line_number_at(&parsed_file.file_content, f.start_byte()),
                logical_line_count(node_string),
            ));
            frame_source_ranges.push((f.start_byte(), f.end_byte()));
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
            frame_source_ranges.push((frame_match.start(), frame_match.end()));
        }
    }
    let frame_labels = frame_labels(&frames);
    let numbered_frame_count = numbered_frame_count(&frame_labels);

    info!(
        "Build: {} -> {} [{}]",
        original_source_path.display(),
        Path::new(&output_file).display(),
        build_mode
    );
    info!(
        "Frames: total={}, numbered={}, parser={} ({} ms)",
        frames.len(),
        numbered_frame_count,
        if !frame_nodes.is_empty() {
            "tree-sitter"
        } else {
            "regex"
        },
        step_start_time.elapsed().as_millis()
    );
    step_start_time = std::time::Instant::now();

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
    let mut preamble = match find {
        Some(x) => Some(parsed_file.file_content[..x].to_owned()),
        None => None,
    }
    .unwrap_or_else(|| r"\documentclass[aspectratio=43,c,xcolor=dvipsnames]{beamer}".to_string());

    let title_info = if !args.is_present("global-title-info") {
        extract_title_commands(&mut preamble)
    } else {
        String::new()
    };

    std::fs::create_dir_all(&cachedir).map_err(|ref err| {
        error!(
            "Failed to create cache dir \"{}\": {}",
            cachedir.display(),
            err
        );
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

    let mut preamble_hash_input = preamble.clone();
    append_latex_cache_key(
        &mut preamble_hash_input,
        selected_engine,
        precompile_preamble,
        &compiler_options,
    );
    let preamble_hash = md5::compute(&preamble_hash_input);
    let preamble_line_count = logical_line_count(&preamble);
    let document_begin_line = find
        .map(|idx| line_number_at(&parsed_file.file_content, idx))
        .unwrap_or(preamble_line_count + 1);
    let document_end_line = parsed_file
        .file_content
        .rfind("\\end{document}")
        .map(|idx| line_number_at(&parsed_file.file_content, idx))
        .unwrap_or(document_begin_line);
    let document_contexts =
        document_contexts_before_frames(&parsed_file.file_content, &frame_source_ranges, find);
    let preamble_filename = preamble_job_name(&preamble_hash);
    let preamble_format_path = input_dir.join(format!("{}.fmt", preamble_filename));
    if !precompile_preamble {
        info!(
            "Preamble: precompilation disabled for {} ({} ms)",
            selected_engine.command_name(),
            step_start_time.elapsed().as_millis()
        );
        step_start_time = std::time::Instant::now();
    } else if preamble_format_path.is_file() && !force_recompile {
        info!(
            "Preamble: cached {} ({} ms)",
            preamble_format_path.display(),
            step_start_time.elapsed().as_millis()
        );
        step_start_time = std::time::Instant::now();
    } else {
        info!("Preamble: compiling {}", preamble_format_path.display());
        if selected_engine == LatexEngine::XeLatex {
            warn!(
                "Preamble precompilation with xelatex is expected to fail for preambles that load native fonts or font mappings; use --no-precompile-preamble if the format build fails."
            );
        }
        let mut command = Command::new(selected_engine.command_name());
        command
            .arg("-shell-escape")
            .arg("-ini")
            .arg(format!("-jobname={}", preamble_filename));
        for option in &compiler_options {
            command.arg(option);
        }
        let output = command
            .arg(format!("&{}", selected_engine.command_name()))
            .arg("mylatexformat.ltx")
            .arg(tex_input_name(&input_path))
            .current_dir(&input_dir)
            .output();
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
                log_command_error(selected_engine.command_name(), "compile the preamble", &e);
                show_error_slide(&cachedir, &output_file, selected_engine);

                *PREVIOUS_FRAMES.lock().unwrap() = Vec::new();
                return Err(FasterBeamerError::CompileError);
            }
            Ok(output) if !output.status.success() => {
                let preamble_log_path = input_dir.join(format!("{}.log", preamble_filename));
                if selected_engine == LatexEngine::LuaLatex
                    && lualatex_format_dump_completed_after_backend_error(
                        &preamble_format_path,
                        &preamble_log_path,
                    )
                {
                    warn!(
                        "Preamble: lualatex reported a PDF backend error after dumping {}; using the generated format.",
                        preamble_format_path.display()
                    );
                    info!(
                        "Preamble: compiled {} ({} ms)",
                        preamble_format_path.display(),
                        step_start_time.elapsed().as_millis()
                    );
                    step_start_time = std::time::Instant::now();
                } else {
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
                    let summary = summarize_command_output(stderr, stdout);
                    error!(
                        "Failed to compile preamble. Details were written to {}.",
                        original_source_path.with_extension("log").display()
                    );
                    if !summary.is_empty() {
                        error!("Reason: {}", summary);
                    }
                    show_error_slide(&cachedir, &output_file, selected_engine);

                    *PREVIOUS_FRAMES.lock().unwrap() = Vec::new();
                    return Err(FasterBeamerError::CompileError);
                }
            }
            _ => {
                info!(
                    "Preamble: compiled {} ({} ms)",
                    preamble_format_path.display(),
                    step_start_time.elapsed().as_millis()
                );
                step_start_time = std::time::Instant::now();
            }
        };
    }

    let source_sections = document_sections(&parsed_file.file_content);
    let mut generated_documents = Vec::new();
    let mut unsupported_dynamic_toc_frames = 0usize;
    let mut command = Command::new("pdfunite");
    for (frame_idx, ((f, (source_frame_start_line, frame_line_count)), document_context)) in frames
        .iter()
        .zip(frame_source_lines.iter())
        .zip(document_contexts.iter())
        .enumerate()
    {
        let format_line = if precompile_preamble {
            format!("%&{}\n", preamble_filename)
        } else {
            String::new()
        };
        let counter_setup = frame_counter_setup(&frame_labels[frame_idx], correct_frame_numbers);
        let number_display_setup = frame_number_display_setup(&frame_labels[frame_idx]);
        let toc_frame_patch = toc_frame_patch(
            f,
            *source_frame_start_line,
            document_begin_line,
            &input_dir,
            &input_path,
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
        let needs_title_info = f.contains("\\titlepage")
            || f.contains("\\maketitle")
            || f.contains("\\inserttitle")
            || f.contains("\\insertauthor")
            || f.contains("\\insertdate")
            || f.contains("\\insertinstitute")
            || f.contains("\\insertsubtitle");

        let mut compile_prefix = format_line.clone() + &preamble + "\n\\begin{document}\n";
        if needs_title_info && !title_info.is_empty() {
            compile_prefix.push_str(&title_info);
            compile_prefix.push('\n');
        }
        let mut context_segments = Vec::new();
        append_document_context(&mut compile_prefix, &mut context_segments, document_context);
        compile_prefix.push_str(&counter_setup);
        compile_prefix.push_str(number_display_setup);
        compile_prefix.push_str(&toc_runtime_setup);
        let compile_string = compile_prefix.clone() + &f + "\n\\end{document}\n";

        let mut hash_input = compile_string.clone();
        for support_file in &support_files {
            hash_input.push_str(support_file.extension);
            hash_input.push('\n');
            hash_input.push_str(&support_file.content);
            hash_input.push('\n');
        }
        append_latex_cache_key(
            &mut hash_input,
            selected_engine,
            precompile_preamble,
            &compiler_options,
        );

        let hash = md5::compute(&hash_input);
        let temp_file_name = frame_temp_file_name(&hash);
        let output = compiled_pdf_path(&cache_subdir, &temp_file_name);
        let temp_frame_start_line = logical_line_count(&compile_prefix) + 1;
        let temp_document_begin_line =
            logical_line_count(&(format_line.clone() + &preamble + "\n")) + 1;
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
        segments.extend(context_segments);
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
            "Detected {} dynamic Beamer TOC frame(s) that faster-beamer cannot render correctly as cached per-frame PDFs (for example \\AtBeginSection with \\tableofcontents[currentsection]). The build will continue, but the proper workflow is a full document compile such as: {} -interaction=nonstopmode -halt-on-error {} ; {} -interaction=nonstopmode -halt-on-error {} (and run bibtex/biber between passes if needed).",
            unsupported_dynamic_toc_frames,
            selected_engine.command_name(),
            tex_input_name(&input_path),
            selected_engine.command_name(),
            tex_input_name(&input_path),
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
    let mut compile_targets = Vec::new();
    let mut cached_frames = 0;

    for (frame_idx, document) in generated_documents.iter().enumerate() {
        if !seen_compile_jobs.insert(document.sync_map.temp_file_name.clone()) {
            continue;
        }

        let compiled_pdf = compiled_pdf_path(&cache_subdir, &document.sync_map.temp_file_name);
        let dependencies = dependencies_for_document(&cache_subdir, document);
        let mut needs_compile =
            force_recompile || !compiled_output_is_fresh(&compiled_pdf, &dependencies);

        let only_frames: Option<Vec<String>> = args.values_of("only-frames").map(|v| v.map(|s| s.to_string()).collect());
        if let Some(filters) = &only_frames {
            if !frame_matches_filter(&frame_labels[frame_idx], &frames[frame_idx], filters) {
                if compiled_pdf.exists() {
                    needs_compile = false;
                } else {
                    warn!(
                        "Frame {} skipped by --only-frames but cached PDF is missing. Compiling anyway to satisfy pdfunite.",
                        frame_label_for_index(&frame_labels, frame_idx, numbered_frame_count)
                    );
                }
            }
        }

        if needs_compile {
            compile_targets.push((frame_idx, document, needs_compile));
        } else {
            trace!(
                "{} is already compiled!",
                compiled_pdf.to_str().unwrap_or("???")
            );
            cached_frames += 1;
        }
    }

    let compile_job_count = compile_targets.len() + cached_frames;
    let frames_to_compile = compile_targets.len();
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
        first_changed_frame_label(first_changed_frame, &frame_labels, numbered_frame_count)
    );
    info!(
        "LaTeX: engine={}, passes={}, bibliography={}, precompile-preamble={}, parallel={} ({} ms)",
        selected_engine.command_name(),
        latex_pass_count,
        bibliography_label(bibliography),
        if precompile_preamble { "on" } else { "off" },
        parallel_label,
        step_start_time.elapsed().as_millis()
    );
    step_start_time = std::time::Instant::now();

    let frame_map: std::sync::Arc<std::sync::Mutex<Vec<(char, FrameLabel)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(
            compile_targets
                .iter()
                .map(|(fi, _, _)| ('.', frame_labels[*fi].clone()))
                .collect(),
        ));
    let progress_bar = compile_progress_bar(compile_targets.len());
    let latex_input = LatexInput::new();
    let compile_failures: Mutex<Vec<FrameCompileFailure>> = Mutex::new(Vec::new());

    let run_document = |frame_idx: usize,
                        document: &GeneratedDocument|
     -> Option<FrameCompileFailure> {
        let (source_start_line, source_line_count) = frame_source_lines[frame_idx];
        let temp_file = input_dir.join(&document.sync_map.temp_file_name);
        let frame_preview = frame_preview(&frames[frame_idx]);
        let mut support_paths = Vec::new();

        if write(&temp_file, &document.tex_content).is_ok() {
            if let Err(err) =
                remove_frame_job_sidecars(&cache_subdir, &document.sync_map.temp_file_name)
            {
                warn!(
                    "Failed to remove stale frame sidecar files for {}: {}",
                    document.sync_map.temp_file_name, err
                );
            }
            if let Err(err) =
                remove_dependency_job_sidecars(&cache_subdir, &input_dir, &document.dependencies)
            {
                warn!(
                    "Failed to remove stale input/include sidecar files for {}: {}",
                    document.sync_map.temp_file_name, err
                );
            }

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
                LatexCompiler::new_in_with_engine(cache_subdir.clone(), selected_engine)
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
            let compile_error = match result {
                Ok(compiled_pdf) => validate_compiled_pdf(&compiled_pdf).err().map(|err| {
                    format!(
                        "Generated PDF {} is incomplete or unreadable: {}",
                        compiled_pdf.display(),
                        err
                    )
                }),
                Err(err) => Some(format!("{}", err)),
            };

            if compile_error.is_none() {
                if update_dependency_manifest(&cache_subdir, &document.sync_map.temp_file_name)
                    .is_err()
                {
                    warn!(
                        "Failed to update dependency manifest for {}",
                        document.sync_map.temp_file_name
                    );
                }
                if let Err(err) = remove_file_if_exists_with_retries(&temp_file) {
                    warn!(
                        "Failed to remove temporary frame source {}: {}",
                        temp_file.display(),
                        err
                    );
                }
                for support_path in &support_paths {
                    if let Err(err) = remove_file_if_exists_with_retries(support_path) {
                        warn!(
                            "Failed to remove temporary support file {}: {}",
                            support_path.display(),
                            err
                        );
                    }
                }
                trace!("Compiled file {}", &temp_file.to_str().unwrap());
                None
            } else {
                for support_path in &support_paths {
                    if let Err(err) = remove_file_if_exists_with_retries(support_path) {
                        warn!(
                            "Failed to remove temporary support file {}: {}",
                            support_path.display(),
                            err
                        );
                    }
                }
                Some(FrameCompileFailure {
                    frame_idx,
                    source_start_line,
                    source_line_count,
                    temp_file: temp_file.clone(),
                    temp_file_name: document.sync_map.temp_file_name.clone(),
                    sync_segments: document.sync_map.segments.clone(),
                    frame_preview,
                    error: compile_error.unwrap(),
                })
            }
        } else {
            Some(FrameCompileFailure {
                frame_idx,
                source_start_line,
                source_line_count,
                temp_file: temp_file.clone(),
                temp_file_name: document.sync_map.temp_file_name.clone(),
                sync_segments: document.sync_map.segments.clone(),
                frame_preview,
                error: String::from("Failed to write generated frame source to disk."),
            })
        }
    };

    let set_compile_job_state =
        |frame_map: &std::sync::Arc<std::sync::Mutex<Vec<(char, FrameLabel)>>>,
         progress_bar: &ProgressBar,
         job_idx: usize,
         state: char| {
            let mut map = frame_map.lock().unwrap();
            map[job_idx].0 = state;
            progress_bar.set_message(render_frame_map(&map));
        };

    let compile_document = |job_idx: usize, frame_idx: usize, document: &GeneratedDocument| {
        set_compile_job_state(&frame_map, &progress_bar, job_idx, 'R');
        let failure = run_document(frame_idx, document);
        set_compile_job_state(
            &frame_map,
            &progress_bar,
            job_idx,
            if failure.is_some() { 'X' } else { '#' },
        );
        if let Some(failure) = failure {
            compile_failures
                .lock()
                .expect("compile_failures lock should not be poisoned")
                .push(failure);
        }
        progress_bar.inc(1);
    };

    if use_parallel {
        if let Some(job_count) = parallel_job_count {
            rayon::ThreadPoolBuilder::new()
                .num_threads(job_count)
                .build()
                .expect("Failed to build the compile thread pool")
                .install(|| {
                    compile_targets.par_iter().enumerate().for_each(
                        |(job_idx, (frame_idx, document, _))| {
                            compile_document(job_idx, *frame_idx, document)
                        },
                    );
                });
        } else {
            compile_targets.par_iter().enumerate().for_each(
                |(job_idx, (frame_idx, document, _))| {
                    compile_document(job_idx, *frame_idx, document)
                },
            );
        }
    } else {
        compile_targets
            .iter()
            .enumerate()
            .for_each(|(job_idx, (frame_idx, document, _))| {
                compile_document(job_idx, *frame_idx, document)
            });
    }
    progress_bar.finish_and_clear();
    info!(
        "Frames compiled ({} ms)",
        step_start_time.elapsed().as_millis()
    );
    step_start_time = std::time::Instant::now();

    let mut failed_compiles = compile_failures
        .into_inner()
        .expect("compile_failures lock should not be poisoned");
    if !failed_compiles.is_empty() && use_parallel {
        let retry_frame_indices: Vec<usize> = failed_compiles
            .iter()
            .map(|failure| failure.frame_idx)
            .collect();
        warn!(
            "{} frame build(s) failed during parallel compilation; retrying them serially.",
            retry_frame_indices.len()
        );

        let retry_frame_map: std::sync::Arc<std::sync::Mutex<Vec<(char, FrameLabel)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(
                retry_frame_indices
                    .iter()
                    .map(|frame_idx| ('.', frame_labels[*frame_idx].clone()))
                    .collect(),
            ));
        let retry_progress_bar = compile_progress_bar(retry_frame_indices.len());
        let mut retry_failures = Vec::new();
        for (job_idx, frame_idx) in retry_frame_indices.iter().enumerate() {
            set_compile_job_state(&retry_frame_map, &retry_progress_bar, job_idx, 'R');
            let failure = run_document(*frame_idx, &generated_documents[*frame_idx]);
            set_compile_job_state(
                &retry_frame_map,
                &retry_progress_bar,
                job_idx,
                if failure.is_some() { 'X' } else { '#' },
            );
            if let Some(failure) = failure {
                retry_failures.push(failure);
            }
            retry_progress_bar.inc(1);
        }
        retry_progress_bar.finish_and_clear();

        if retry_failures.is_empty() {
            info!(
                "Serial retry recovered {} frame build(s).",
                retry_frame_indices.len()
            );
            failed_compiles.clear();
        } else {
            warn!(
                "Serial retry still failed for {} of {} frame build(s).",
                retry_failures.len(),
                retry_frame_indices.len()
            );
            failed_compiles = retry_failures;
        }
    }
    if !failed_compiles.is_empty() {
        if let Err(err) = write_master_log_for_frame_failures(
            &failed_compiles,
            &cache_subdir,
            &original_source_path,
            &frame_labels,
            numbered_frame_count,
        ) {
            let _ = err;
            warn!(
                "Failed to create aggregated source log for {}.",
                original_source_path.display()
            );
        }
        log_frame_compile_failures(
            &failed_compiles,
            &original_source_path,
            &cache_subdir,
            &frame_labels,
            numbered_frame_count,
        );
        show_error_slide(&cachedir, &output_file, selected_engine);
        *PREVIOUS_FRAMES.lock().unwrap() = Vec::new();
        return Err(FasterBeamerError::CompileError);
    }

    let pdfunite_with_synctex = args.is_present("pdfunite-synctex");
    if args.is_present("pdfunite") || pdfunite_with_synctex {
        let merged_pdf = cache_subdir.join(PDFUNITE_TEMP_FILE);
        let publish_label = if pdfunite_with_synctex {
            "pdfunite + synctex"
        } else {
            "pdfunite"
        };
        info!(
            "Publish: {} -> {} ({} ms)",
            publish_label,
            Path::new(&output_file).display(),
            step_start_time.elapsed().as_millis()
        );
        step_start_time = std::time::Instant::now();
        let output = command.arg(&merged_pdf).output();

        match output {
            Err(e) => {
                if e.kind() == ErrorKind::NotFound {
                    error!(
                        "Failed to run pdfunite: pdfunite was not found on PATH. Install it or use --tex-unite instead."
                    );
                } else {
                    error!("Failed to run pdf unite!\n{}", e);
                }
                show_error_slide(&cachedir, &output_file, selected_engine);

                *PREVIOUS_FRAMES.lock().unwrap() = frames;
                return Err(FasterBeamerError::PdfUniteError);
            }
            Ok(output) if !output.status.success() => {
                error!(
                    "Failed to run pdfunite! {}",
                    str::from_utf8(&output.stderr).unwrap()
                );
                show_error_slide(&cachedir, &output_file, selected_engine);

                *PREVIOUS_FRAMES.lock().unwrap() = frames;
                return Err(FasterBeamerError::PdfUniteError);
            }
            Ok(_) => {
                if pdfunite_with_synctex {
                    match build_merged_frame_synctex(
                        &generated_documents,
                        &cache_subdir,
                        &output_file,
                    ) {
                        Ok((merged_synctex, page_count)) => {
                            publish_output_file(&merged_pdf, &output_file)?;
                            publish_synctex_contents(&merged_synctex, &output_file)?;
                            info!(
                                "SyncTeX: merged {} page(s) from frame sidecars; skipped united TeX compile.",
                                page_count
                            );
                        }
                        Err(merge_error) => {
                            warn!(
                                "Failed to merge frame SyncTeX directly ({}); falling back to a temporary united TeX build.",
                                merge_error
                            );

                            match compile_united_artifacts(
                                &parsed_file.file_content,
                                &frames,
                                &frame_source_lines,
                                &generated_documents,
                                &cache_subdir,
                                &original_source_path,
                                &input_dir,
                                selected_engine,
                                &compiler_options,
                                run_options,
                            ) {
                                Ok(united) => {
                                    publish_output_file(&merged_pdf, &output_file)?;
                                    rewrite_synctex_to_original(
                                        &united.pdf_file,
                                        &united.sync_map,
                                    )?;
                                    publish_synctex_file(&united.pdf_file, &output_file)?;
                                    if let Err(err) = std::fs::remove_file(&united.tex_file) {
                                        warn!(
                                            "Failed to remove temporary united source {}: {}",
                                            united.tex_file.display(),
                                            err
                                        );
                                    }
                                }
                                Err(err) => {
                                    publish_output_artifacts(&merged_pdf, &output_file, None)?;
                                    warn!(
                                        "Published the pdfunite output without SyncTeX because the temporary united TeX build failed."
                                    );

                                    *PREVIOUS_FRAMES.lock().unwrap() = frames;
                                    return Err(err);
                                }
                            }
                        }
                    }
                } else {
                    publish_output_artifacts(&merged_pdf, &output_file, None)?;
                }
            }
        };
    } else if args.is_present("tex-unite") {
        info!(
            "Publish: united document -> {} ({} ms)",
            Path::new(&output_file).display(),
            step_start_time.elapsed().as_millis()
        );
        step_start_time = std::time::Instant::now();

        match compile_united_artifacts(
            &parsed_file.file_content,
            &frames,
            &frame_source_lines,
            &generated_documents,
            &cache_subdir,
            &original_source_path,
            &input_dir,
            selected_engine,
            &compiler_options,
            run_options,
        ) {
            Ok(united) => {
                publish_output_artifacts(&united.pdf_file, &output_file, Some(&united.sync_map))?;
                if let Err(err) = std::fs::remove_file(&united.tex_file) {
                    warn!(
                        "Failed to remove temporary united source {}: {}",
                        united.tex_file.display(),
                        err
                    );
                }
            }
            Err(err) => {
                show_error_slide(&cachedir, &output_file, selected_engine);

                *PREVIOUS_FRAMES.lock().unwrap() = frames;
                return Err(err);
            }
        }
    } else {
        if first_changed_frame == generated_documents.len() {
            first_changed_frame = 0;
        }
        if first_changed_frame < generated_documents.len() {
            info!(
                "Publish: preview frame {} -> {} ({} ms)",
                frame_label_for_index(&frame_labels, first_changed_frame, numbered_frame_count),
                Path::new(&output_file).display(),
                step_start_time.elapsed().as_millis()
            );
            step_start_time = std::time::Instant::now();
            let document = &generated_documents[first_changed_frame];
            let compiled_pdf = compiled_pdf_path(&cache_subdir, &document.sync_map.temp_file_name);

            if Path::new(&compiled_pdf).is_file() {
                publish_output_artifacts(&compiled_pdf, &output_file, Some(&document.sync_map))?;
            } else {
                error!("Compilation failed!");
                show_error_slide(&cachedir, &output_file, selected_engine);

                *PREVIOUS_FRAMES.lock().unwrap() = frames;
                return Err(FasterBeamerError::CompileError);
            }
        }
    }

    *PREVIOUS_FRAMES.lock().unwrap() = frames;
    info!(
        "Total time: {} ms (last step {} ms)",
        total_start_time.elapsed().as_millis(),
        step_start_time.elapsed().as_millis()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::build_merged_frame_synctex;
    use super::collect_related_files;
    use super::document_contexts_before_frames;
    use super::document_sections;
    use super::first_changed_frame_index;
    use super::frame_counter_setup;
    use super::frame_labels;
    use super::frame_number_display_setup;
    use super::numbered_frame_count;
    use super::resolve_input_file;
    use super::toc_frame_patch;
    use super::united_frame_replacement;
    use super::FrameLabel;
    use super::FrameSyncTexMap;
    use super::GeneratedDocument;
    use super::SyncTexLineSegment;
    use super::TocFrameSupport;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::fs;
    use std::io::Write;
    use std::path::Path;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    fn regex_frame_ranges(source: &str) -> Vec<(usize, usize)> {
        super::FRAME_REGEX
            .captures_iter(source)
            .map(|capture| {
                let frame_match = capture.get(0).unwrap();
                (frame_match.start(), frame_match.end())
            })
            .collect()
    }

    fn context_text(context: &[super::DocumentContextSnippet]) -> String {
        context
            .iter()
            .map(|snippet| snippet.content.as_str())
            .collect::<Vec<_>>()
            .join("")
    }

    fn write_synctex_fixture(path: &Path, content: &str) {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(content.as_bytes()).unwrap();
        fs::write(path, encoder.finish().unwrap()).unwrap();
    }

    #[test]
    fn frame_number_setup_uses_previous_numbered_frame() {
        assert_eq!(
            frame_counter_setup(&FrameLabel::Number(3), true),
            "\\setcounter{framenumber}{2}\n"
        );
        assert_eq!(
            frame_counter_setup(&FrameLabel::Title, true),
            "\\setcounter{framenumber}{0}\n"
        );
        assert_eq!(
            frame_counter_setup(&FrameLabel::Toc, true),
            "\\setcounter{framenumber}{0}\n"
        );
    }

    #[test]
    fn frame_number_setup_is_empty_when_disabled() {
        assert_eq!(frame_counter_setup(&FrameLabel::Number(3), false), "");
    }

    #[test]
    fn toc_frame_display_setup_hides_frame_number() {
        let setup = frame_number_display_setup(&FrameLabel::Toc);

        assert!(setup.contains("\\def\\insertframenumber{}"));
        assert!(setup.contains("\\setbeamertemplate{page number in head/foot}{}"));
        assert!(setup.contains("\\setbeamertemplate{frame numbering}{}"));
        assert_eq!(frame_number_display_setup(&FrameLabel::Number(1)), "");
    }

    #[test]
    fn frame_labels_skip_front_matter_title_and_toc_frames() {
        let labels = frame_labels(&[
            String::from("\\begin{frame}\\titlepage\\end{frame}"),
            String::from("\\begin{frame}{Agenda}\\tableofcontents\\end{frame}"),
            String::from("\\begin{frame}{Body}\\end{frame}"),
            String::from("\\sectiontitlepage{A}{B}"),
            String::from("\\begin{frame}{More}\\end{frame}"),
            String::from("\\begin{frame}{Roadmap}\\tableofcontents[currentsection]\\end{frame}"),
        ]);

        assert_eq!(
            labels,
            vec![
                FrameLabel::Title,
                FrameLabel::Toc,
                FrameLabel::Number(1),
                FrameLabel::Number(2),
                FrameLabel::Number(3),
                FrameLabel::Number(4)
            ]
        );
        assert_eq!(numbered_frame_count(&labels), 4);
    }

    #[test]
    fn frame_labels_start_after_title_when_toc_is_absent() {
        let labels = frame_labels(&[
            String::from("\\begin{frame}\\titlepage\\end{frame}"),
            String::from("\\sectiontitlepage{Intro}{Overview}"),
            String::from("\\begin{frame}{Body}\\end{frame}"),
        ]);

        assert_eq!(
            labels,
            vec![
                FrameLabel::Title,
                FrameLabel::Number(1),
                FrameLabel::Number(2)
            ]
        );
        assert_eq!(numbered_frame_count(&labels), 2);
    }

    #[test]
    fn document_context_accumulates_definitions_between_frames() {
        let source = "\\documentclass{beamer}\n\
\\begin{document}\n\
\\begin{frame}{A}\n\
A\n\
\\end{frame}\n\
\\newcommand{\\shared}{first}\n\
\\definecolor{brand}{RGB}{1,2,3}\n\
\\section{Ignored}\n\
\\begin{frame}{B}\n\
\\shared\n\
\\end{frame}\n\
\\renewcommand{\\shared}{second}\n\
\\begin{frame}{C}\n\
\\shared\n\
\\end{frame}\n\
\\end{document}\n";

        let contexts = document_contexts_before_frames(
            source,
            &regex_frame_ranges(source),
            source.find("\\begin{document}"),
        );

        assert_eq!(context_text(&contexts[0]), "");

        let second_context = context_text(&contexts[1]);
        assert!(second_context.contains("\\newcommand{\\shared}{first}"));
        assert!(second_context.contains("\\definecolor{brand}{RGB}{1,2,3}"));
        assert!(!second_context.contains("\\section{Ignored}"));
        assert_eq!(contexts[1][0].source_start_line, 6);

        let third_context = context_text(&contexts[2]);
        assert!(third_context.contains("\\newcommand{\\shared}{first}"));
        assert!(third_context.contains("\\renewcommand{\\shared}{second}"));
    }

    #[test]
    fn document_context_keeps_multiline_definitions_together() {
        let source = "\\documentclass{beamer}\n\
\\begin{document}\n\
\\begin{frame}{A}\n\
A\n\
\\end{frame}\n\
\\newcommand{\\wrapped}[1]{%\n\
  \\textbf{#1}\n\
}\n\
% \\newcommand{\\commented}{ignored}\n\
\\begin{frame}{B}\n\
\\wrapped{B}\n\
\\end{frame}\n\
\\end{document}\n";

        let contexts = document_contexts_before_frames(
            source,
            &regex_frame_ranges(source),
            source.find("\\begin{document}"),
        );
        let second_context = context_text(&contexts[1]);

        assert!(second_context.contains("\\newcommand{\\wrapped}[1]{%"));
        assert!(second_context.contains("\\textbf{#1}"));
        assert!(second_context.contains("}\n"));
        assert!(!second_context.contains("\\commented"));
        assert_eq!(contexts[1].len(), 1);
        assert_eq!(contexts[1][0].source_start_line, 6);
    }

    #[test]
    fn resolve_input_file_appends_tex_when_bare_input_is_missing() {
        let temp_dir = tempdir().unwrap();
        let bare_input = temp_dir.path().join("slides");
        let tex_input = temp_dir.path().join("slides.tex");
        fs::write(&tex_input, "\\documentclass{beamer}").unwrap();

        assert_eq!(resolve_input_file(bare_input.to_str().unwrap()), tex_input);
    }

    #[test]
    fn resolve_input_file_keeps_existing_bare_input() {
        let temp_dir = tempdir().unwrap();
        let bare_input = temp_dir.path().join("slides");
        fs::write(&bare_input, "\\documentclass{beamer}").unwrap();

        assert_eq!(resolve_input_file(bare_input.to_str().unwrap()), bare_input);
    }

    #[test]
    fn resolve_input_file_keeps_explicit_tex_input() {
        let temp_dir = tempdir().unwrap();
        let tex_input = temp_dir.path().join("slides.tex");

        assert_eq!(resolve_input_file(tex_input.to_str().unwrap()), tex_input);
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
    fn merged_frame_synctex_combines_pages_and_remaps_input_tags() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path();
        let source_file = temp_dir.path().join("slides.tex");
        let document_a = GeneratedDocument {
            tex_content: String::new(),
            sync_map: FrameSyncTexMap {
                source_file: source_file.clone(),
                temp_file_name: String::from("frame-a.tex"),
                segments: vec![SyncTexLineSegment {
                    temp_start_line: 10,
                    line_count: 3,
                    source_start_line: 100,
                }],
            },
            dependencies: Vec::new(),
            support_files: Vec::new(),
        };
        let document_b = GeneratedDocument {
            tex_content: String::new(),
            sync_map: FrameSyncTexMap {
                source_file: source_file.clone(),
                temp_file_name: String::from("frame-b.tex"),
                segments: vec![SyncTexLineSegment {
                    temp_start_line: 5,
                    line_count: 2,
                    source_start_line: 200,
                }],
            },
            dependencies: Vec::new(),
            support_files: Vec::new(),
        };

        write_synctex_fixture(
            &cache_dir.join("frame-a.synctex.gz"),
            "SyncTeX Version:1\n\
Input:1:frame-a.tex\n\
Input:2:common.sty\n\
Output:frame-a.pdf\n\
Magnification:1000\n\
Unit:1\n\
X Offset:0\n\
Y Offset:0\n\
Content:\n\
!944\n\
{1\n\
[1,10:0,0:0,0,0\n\
x2,3:0,0\n\
!97\n\
}1\n\
Postamble:\n\
Count:10\n\
!123\n\
Post scriptum:\n",
        );
        write_synctex_fixture(
            &cache_dir.join("frame-b.synctex.gz"),
            "SyncTeX Version:1\n\
Input:7:frame-b.tex\n\
Input:8:common.sty\n\
Output:frame-b.pdf\n\
Magnification:1000\n\
Unit:1\n\
X Offset:0\n\
Y Offset:0\n\
Content:\n\
!120\n\
{1\n\
(7,5:0,0:0,0,0\n\
x8,4:0,0\n\
g9,4:0,0\n\
r9,4:0,0:0,0,0\n\
Input:9:late.sty\n\
}1\n\
!240\n\
{2\n\
[7,6:0,0:0,0,0\n\
}2\n\
Postamble:\n\
Count:20\n\
!456\n\
Post scriptum:\n",
        );

        let (merged, page_count) =
            build_merged_frame_synctex(&[document_a, document_b], cache_dir, "merged.pdf").unwrap();
        let source_path = super::synctex_path(&source_file);

        assert_eq!(page_count, 3);
        assert_eq!(
            merged.matches(&format!("Input:1:{}", source_path)).count(),
            1
        );
        assert_eq!(merged.matches("Input:2:common.sty").count(), 1);
        assert_eq!(merged.matches("Input:3:late.sty").count(), 1);
        assert!(merged.contains("Output:merged.pdf"));
        assert!(merged.contains("\n{1\n"));
        assert!(merged.contains("\n{2\n"));
        assert!(merged.contains("\n{3\n"));
        assert!(merged.contains("\n}1\n"));
        assert!(merged.contains("\n}2\n"));
        assert!(merged.contains("\n}3\n"));
        assert!(merged.contains("[1,100:0,0:0,0,0"));
        assert!(merged.contains("(1,200:0,0:0,0,0"));
        assert!(merged.contains("[1,201:0,0:0,0,0"));
        assert!(merged.contains("x2,3:0,0"));
        assert!(merged.contains("x2,4:0,0"));
        assert!(merged.contains("g3,4:0,0"));
        assert!(merged.contains("r3,4:0,0:0,0,0"));
        assert!(merged.contains("Count:30"));
        assert!(!merged.contains("frame-a.tex"));
        assert!(!merged.contains("frame-b.tex"));
    }

    #[test]
    fn merged_frame_synctex_reports_missing_sidecar() {
        let temp_dir = tempdir().unwrap();
        let source_file = temp_dir.path().join("slides.tex");
        let document = GeneratedDocument {
            tex_content: String::new(),
            sync_map: FrameSyncTexMap {
                source_file,
                temp_file_name: String::from("missing.tex"),
                segments: Vec::new(),
            },
            dependencies: Vec::new(),
            support_files: Vec::new(),
        };

        let error =
            build_merged_frame_synctex(&[document], temp_dir.path(), "merged.pdf").unwrap_err();

        assert!(error.contains("failed to read"));
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

        let dependencies =
            collect_related_files("\\graphicspath{{./figs/}}\\input{shared}", temp_dir.path());

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
                assert!(patch.support_files[0]
                    .content
                    .contains("\\beamer@sectionintoc {1}{Intro}"));
                assert!(patch.support_files[0]
                    .content
                    .contains("\\beamer@sectionintoc {2}{Next}"));
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
    fn remove_frame_job_sidecars_clears_stale_auxiliary_files() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path();
        let temp_file_name = "faster-beamer-temp-test.tex";
        let aux_path = cache_dir.join("faster-beamer-temp-test.aux");
        let toc_path = cache_dir.join("faster-beamer-temp-test.toc");
        let synctex_path = cache_dir.join("faster-beamer-temp-test.synctex.gz");
        let pdf_path = cache_dir.join("faster-beamer-temp-test.pdf");
        let deps_path = cache_dir.join("faster-beamer-temp-test.deps");

        fs::write(&aux_path, b"\0\0\0").unwrap();
        fs::write(&toc_path, "toc").unwrap();
        fs::write(&synctex_path, "synctex").unwrap();
        fs::write(&pdf_path, "pdf").unwrap();
        fs::write(&deps_path, "deps").unwrap();

        super::remove_frame_job_sidecars(cache_dir, temp_file_name).unwrap();

        assert!(!aux_path.exists());
        assert!(!toc_path.exists());
        assert!(!synctex_path.exists());
        assert!(pdf_path.exists());
        assert!(deps_path.exists());
    }

    #[test]
    fn remove_dependency_job_sidecars_clears_input_auxiliary_files() {
        let temp_dir = tempdir().unwrap();
        let input_dir = temp_dir.path().join("deck");
        let cache_dir = temp_dir.path().join("cache");
        fs::create_dir_all(&input_dir).unwrap();
        fs::create_dir_all(&cache_dir).unwrap();
        let input_path = input_dir.join("shared.tex");
        let aux_path = cache_dir.join("shared.aux");
        let toc_path = cache_dir.join("shared.toc");
        let tex_path = cache_dir.join("shared.tex");

        fs::write(&input_path, "shared").unwrap();
        fs::write(&aux_path, "aux").unwrap();
        fs::write(&toc_path, "toc").unwrap();
        fs::write(&tex_path, "tex").unwrap();

        super::remove_dependency_job_sidecars(&cache_dir, &input_dir, &[input_path]).unwrap();

        assert!(!aux_path.exists());
        assert!(!toc_path.exists());
        assert!(tex_path.exists());
    }

    #[test]
    fn remove_dependency_job_sidecars_preserves_relative_subdirectories() {
        let temp_dir = tempdir().unwrap();
        let input_dir = temp_dir.path().join("deck");
        let cache_dir = temp_dir.path().join("cache");
        let include_dir = input_dir.join("parts");
        let cached_include_dir = cache_dir.join("parts");
        fs::create_dir_all(&include_dir).unwrap();
        fs::create_dir_all(&cached_include_dir).unwrap();
        let input_path = include_dir.join("chapter.tex");
        let aux_path = cached_include_dir.join("chapter.aux");

        fs::write(&input_path, "chapter").unwrap();
        fs::write(&aux_path, "aux").unwrap();

        super::remove_dependency_job_sidecars(&cache_dir, &input_dir, &[input_path]).unwrap();

        assert!(!aux_path.exists());
    }

    #[test]
    fn stale_cache_cleanup_preserves_active_cache_subdir() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path().join("faster-beamer");
        let active_cache = cache_dir.join("active").join("deck");
        let stale_cache = cache_dir.join("old").join("deck");
        let active_pdf = active_cache.join("frame.pdf");
        let stale_pdf = stale_cache.join("frame.pdf");

        fs::create_dir_all(&active_cache).unwrap();
        fs::create_dir_all(&stale_cache).unwrap();
        fs::write(&active_pdf, b"active").unwrap();
        fs::write(&stale_pdf, b"stale").unwrap();

        let removed = super::remove_stale_cache_entries(
            &cache_dir,
            &active_cache,
            std::time::SystemTime::now() + Duration::from_secs(1),
        )
        .unwrap();

        assert!(removed >= 2);
        assert!(active_pdf.exists());
        assert!(!stale_pdf.exists());
        assert!(!stale_cache.exists());
    }

    #[test]
    fn cache_sweep_stamp_defers_repeat_cleanup() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path();

        assert!(super::cache_sweep_is_due(cache_dir));
        super::mark_cache_sweep(cache_dir);
        assert!(!super::cache_sweep_is_due(cache_dir));
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

    #[test]
    fn compiled_output_is_not_fresh_when_pdf_is_incomplete() {
        let temp_dir = tempdir().unwrap();
        let dependency = temp_dir.path().join("plot.pdf");
        let compiled_pdf = temp_dir.path().join("frame.pdf");

        fs::write(&dependency, b"dependency").unwrap();
        fs::write(&compiled_pdf, b"%PDF-1.5\nmissing trailer").unwrap();

        assert!(!super::compiled_output_is_fresh(
            &compiled_pdf,
            &[dependency]
        ));
    }

    #[test]
    fn lualatex_format_dump_accepts_backend_failure_after_dump() {
        let temp_dir = tempdir().unwrap();
        let format_path = temp_dir.path().join("preamble.fmt");
        let log_path = temp_dir.path().join("preamble.log");

        fs::write(&format_path, b"format").unwrap();
        fs::write(
            &log_path,
            "Beginning to dump on file preamble.fmt\n! error:  (pdf backend): already written content discarded, no output file produced.",
        )
        .unwrap();

        assert!(super::lualatex_format_dump_completed_after_backend_error(
            &format_path,
            &log_path,
        ));
    }

    #[test]
    fn lualatex_format_dump_rejects_failure_before_dump() {
        let temp_dir = tempdir().unwrap();
        let format_path = temp_dir.path().join("preamble.fmt");
        let log_path = temp_dir.path().join("preamble.log");

        fs::write(&format_path, b"format").unwrap();
        fs::write(
            &log_path,
            "! error:  (pdf backend): already written content discarded, no output file produced.",
        )
        .unwrap();

        assert!(!super::lualatex_format_dump_completed_after_backend_error(
            &format_path,
            &log_path,
        ));
    }

    #[test]
    fn lualatex_parallel_auto_is_capped() {
        assert_eq!(
            super::effective_parallel_job_count(super::LatexEngine::LuaLatex, true, None),
            Some(super::LUALATEX_AUTO_PARALLEL_JOBS)
        );
    }

    #[test]
    fn explicit_parallel_job_count_overrides_lualatex_cap() {
        assert_eq!(
            super::effective_parallel_job_count(super::LatexEngine::LuaLatex, true, Some(8)),
            Some(8)
        );
    }
}
