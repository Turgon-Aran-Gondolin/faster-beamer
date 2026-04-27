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
use indicatif::ProgressBar;
use rayon::prelude::*;
use regex::Regex;
use std::collections::HashSet;
use std::env::current_dir;
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
    hash: md5::Digest,
    tex_content: String,
    sync_map: FrameSyncTexMap,
}

lazy_static! {
    static ref FRAME_REGEX: Regex =
        Regex::new(r"(?ms)^[ \t]*\\begin\{frame\}.*?^[ \t]*\\end\{frame\}").unwrap();
}
lazy_static! {
    static ref DOCUMENT_REGEX: Regex =
        Regex::new(r"(?ms)^[ \t]*\\begin\{document\}.*^[ \t]*\\end\{document\}").unwrap();
}

lazy_static! {
    static ref PREVIOUS_FRAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());
}

const FRAME_TEMP_PREFIX: &str = "faster-beamer-temp-";
const PREAMBLE_TEMP_PREFIX: &str = "faster-beamer-preamble-";
const UNITED_TEMP_PREFIX: &str = "faster-beamer-united-";

fn frame_counter_setup(frame_idx: usize, correct_frame_numbers: bool) -> String {
    if correct_frame_numbers {
        format!("\\setcounter{{framenumber}}{{{}}}\n", frame_idx)
    } else {
        String::new()
    }
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
    info!("Publishing: {:?} -> {:?}", compiled_pdf, output_file);
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
    info!("Publishing: {:?} -> {:?}", compiled_synctex, output_synctex);
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
            info!("Found {} frames with tree-sitter.", frame_nodes.len());
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
    info!("Found {} frames.", frames.len());

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
    if input_dir.join(format!("{}.fmt", preamble_filename)).is_file()
    {
        info!("Precompiled preamble already exists");
    } else {
        info!(
            "Precompiling preamble {:?}",
            input_path.join(format!("{}.fmt", preamble_filename))
        );
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
                log_command_error("pdflatex", "compile the preamble", &e);
                show_error_slide(&cachedir, &output_file);

                *PREVIOUS_FRAMES.lock().unwrap() = Vec::new();
                return Err(FasterBeamerError::CompileError);
            }
            Ok(output) if !output.status.success() => {
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

    let mut generated_documents = Vec::new();
    let mut command = Command::new("pdfunite");
    for (frame_idx, (f, (source_frame_start_line, frame_line_count))) in frames
        .iter()
        .zip(frame_source_lines.iter())
        .enumerate()
    {
        let format_line = format!("%&{}\n", preamble_filename);
        let counter_setup = frame_counter_setup(frame_idx, correct_frame_numbers);
        let compile_prefix = format_line.clone() + &preamble + "\n\\begin{document}\n" + &counter_setup;
        let compile_string = compile_prefix.clone() + &f + "\n\\end{document}\n";

        let hash = md5::compute(&compile_string);
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

        generated_documents.push(GeneratedDocument {
            hash,
            tex_content: compile_string,
            sync_map: FrameSyncTexMap {
                source_file: original_source_path.clone(),
                temp_file_name,
                segments,
            },
        });

        command.arg(&output);
    }

    trace!("Comparing frames");
    let mut first_changed_frame = 0;
    for frame_pair in frames.iter().zip((*PREVIOUS_FRAMES.lock().unwrap()).iter()) {
        match frame_pair {
            (lhs, rhs) if lhs != rhs => {
                break;
            }
            _ => first_changed_frame += 1,
        }
    }
    debug!(
        "Found first difference in frame {} from {}",
        &first_changed_frame,
        frames.len()
    );

    let mut seen_compile_jobs = HashSet::new();
    let compile_targets: Vec<(usize, &GeneratedDocument)> = generated_documents
        .iter()
        .enumerate()
        .filter(|(_, document)| seen_compile_jobs.insert(document.sync_map.temp_file_name.clone()))
        .collect();

    let progress_bar = ProgressBar::new(compile_targets.len() as u64);
    let latex_input = LatexInput::new();

        let compile_document = |frame_idx: usize, document: &GeneratedDocument| {
            let pdf = compiled_pdf_path(&cache_subdir, &document.sync_map.temp_file_name);

            if pdf.is_file() && !force_recompile {
                trace!("{} is already compiled!", pdf.to_str().unwrap_or("???"));
            } else {
                let temp_file = input_dir.join(&document.sync_map.temp_file_name);

                if write(&temp_file, &document.tex_content).is_ok() {
                    let compiler = apply_compiler_options(
                        LatexCompiler::new_in(cache_subdir.clone())
                            .add_arg("-shell-escape")
                            .add_arg("-interaction=nonstopmode")
                            .with_current_dir(input_dir.clone()),
                        &compiler_options,
                    );

                    let result = compiler.run(
                        Path::new(tex_input_name(&temp_file)),
                        &latex_input,
                        run_options,
                    );
                    if result.is_ok() {
                        if let Err(err) = std::fs::remove_file(&temp_file) {
                            warn!("Failed to remove temporary frame source {}: {}", temp_file.display(), err);
                        }
                        trace!("Compiled file {}", &temp_file.to_str().unwrap());
                    } else {
                        error!(
                            "Failed to compile frame {} ({})",
                            frame_idx,
                            &temp_file.to_str().unwrap()
                        );
                        error!("{}", frames[frame_idx]);
                        error!("{}", result.err().unwrap());
                    };
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
                        .for_each(|(frame_idx, document)| compile_document(*frame_idx, document));
                });
        } else {
            compile_targets
                .par_iter()
                .for_each(|(frame_idx, document)| compile_document(*frame_idx, document));
        }
    } else {
        compile_targets
            .iter()
            .for_each(|(frame_idx, document)| compile_document(*frame_idx, document));
    }
    progress_bar.finish_and_clear();

    if args.is_present("pdfunite") {
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
        info!("Pasting precompiled frames into original document!");

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
                LatexCompiler::new_in(cache_subdir)
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

            if compile_result.is_err() {
                error!(
                    "Failed to run pdf unite!\n{}",
                    compile_result.err().unwrap()
                );
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
    use super::frame_counter_setup;
    use super::united_frame_replacement;

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
}
