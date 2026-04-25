//
// process_file.rs
// Copyright (C) 2019 seitz_local <seitz_local@lmeXX>
// Distributed under terms of the GPLv3 license.
//
use crate::beamer::get_frames;
use crate::fs_utils::{cache_path, publish_file};
use crate::latexcompile::BibliographyTool;
use crate::parsing;

use log::Level::Trace;

use crate::latexcompile::{LatexCompiler, LatexInput, LatexRunOptions};
use clap::ArgMatches;
use indicatif::ProgressBar;
use rayon::prelude::*;
use regex::Regex;
use std::env::current_dir;
use std::fs::write;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;
use std::str;
use std::sync::Mutex;
use std::vec::Vec;

#[derive(PartialEq)]
pub enum FasterBeamerError {
    InputFileNotExistent,
    IoError,
    CompileError,
    PdfUniteError,
}

pub type Result<T> = ::std::result::Result<T, FasterBeamerError>;

lazy_static! {
    static ref FRAME_REGEX: Regex =
        Regex::new(r"(?ms)^[\s\t]*?\\begin\{frame\}.*?^[\s\t]*?\\end\{frame\}").unwrap();
}
lazy_static! {
    static ref DOCUMENT_REGEX: Regex =
        Regex::new(r"(?ms)^[\s\t]*?\\begin\{document\}.*^[\s\t]*?\\end\{document\}").unwrap();
}

lazy_static! {
    static ref PREVIOUS_FRAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());
}

fn show_error_slide(cachedir: &Path, output_file: &str) {
    let error_frame = String::from_utf8_lossy(include_bytes!("error.tex")).to_owned();
    let error_file = cachedir.join("error.tex");
    let error_pdf = cachedir.join("error.pdf");

    if !error_pdf.exists() && write(&error_file, &error_frame[..]).is_ok() {
        let mut compiler = LatexCompiler::new()
            .unwrap()
            .add_arg("-shell-escape")
            .add_arg("-interaction=nonstopmode");
        compiler.working_dir = cachedir.to_owned();

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

fn tex_input_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .expect("Expected a TeX input file name")
}

fn default_output_file(input_path: &Path) -> String {
    input_path.with_extension("pdf").to_string_lossy().into_owned()
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
    let cwd = current_dir().unwrap();
    let input_path = Path::new(&input_file);
    let input_dir = input_path
        .parent()
        .unwrap_or(&cwd)
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_owned());
    let output_file = args
        .value_of("OUTPUT")
        .map(|output| output.to_owned())
        .unwrap_or_else(|| default_output_file(input_path));
    let correct_frame_numbers = args.is_present("frame-numbers");
    let latex_pass_count = latex_pass_count(args);
    let bibliography = bibliography_tool(args);
    let run_options = latex_run_options(latex_pass_count, bibliography);
    let force_recompile = args.is_present("force-recompile");

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
    if !frame_nodes.is_empty() {
        for f in frame_nodes.iter() {
            info!("Found {} frames with tree-sitter.", frame_nodes.len());
            let node_string = parsed_file.get_node_string(&f);
            frames.push(node_string.to_string());
        }
    } else {
        for cap in FRAME_REGEX.captures_iter(&parsed_file.file_content) {
            let frame_string = cap[0].to_string();
            trace!("Frame {}:\n{}", frames.len() + 1, &frame_string);
            frames.push(frame_string);
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

    let cachedir = dirs::cache_dir().expect("This OS is not supported").join("faster-beamer");
    std::fs::create_dir_all(&cachedir).map_err(|ref err| {
        error!("Failed to create cache dir \"{}\": {}", cachedir.display(), err);
        FasterBeamerError::IoError
    })?;

    let cache_subdir = cache_path(&cachedir, &input_dir);
    std::fs::create_dir_all(&cache_subdir).map_err(|ref err| {
        error!(
            "Failed to create cache subdir \"{}\": {}",
            cache_subdir.display(),
            err
        );
        FasterBeamerError::IoError
    })?;

    let preamble_hash = md5::compute(&preamble);
    let preamble_filename = format!("{:x}_{}", preamble_hash, args.is_present("draft"));
    if input_path
        .parent()
        .unwrap()
        .join(format!("{}.fmt", preamble_filename))
        .is_file()
    {
        info!("Precompiled preamble already exists");
    } else {
        info!(
            "Precompiling preamble {:?}",
            input_path.join(format!("{}.fmt", preamble_filename))
        );
        let output = Command::new("pdflatex")
            .arg("-shell-escape")
            .arg("-ini")
            .arg(format!("-jobname={}", preamble_filename))
            .arg("&pdflatex")
            .arg("mylatexformat.ltx")
            .arg(tex_input_name(input_path))
            .current_dir(&input_dir)
            .output();
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
    for (frame_idx, f) in frames.iter().enumerate() {
        let frame_idx_str = if correct_frame_numbers {
            format!("{}", frame_idx)
        } else {
            format!("{}", 0)
        };
        let compile_string = format!("%&{}\n", preamble_filename)
            + &preamble
            + "\n\\begin{document}\n"
            + "\\addtocounter{framenumber}{"
            + &frame_idx_str
            + "}\n"
            + &f
            + "\n\\end{document}\n";

        let hash = md5::compute(&compile_string);
        let output = cache_subdir.join(format!("{:x}.pdf", hash));
        generated_documents.push((hash, compile_string));

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

    let progress_bar = ProgressBar::new(generated_documents.len() as u64);

    generated_documents
        .par_iter()
        .enumerate()
        .for_each(|(frame_idx, (hash, tex_content))| {
            let pdf = cache_subdir.join(format!("{:x}.pdf", hash));

            if pdf.is_file() && !force_recompile {
                trace!("{} is already compiled!", pdf.to_str().unwrap_or("???"));
            } else {
                let latex_input = LatexInput::from_lazy(&input_dir, &cachedir)
                    .expect("Failed to create LatexInput");

                let temp_file = input_dir.join(format!("{:x}.tex", hash));

                if write(&temp_file, &tex_content).is_ok() {
                    let mut compiler = LatexCompiler::new()
                        .unwrap()
                        .add_arg("-shell-escape")
                        .add_arg("-interaction=nonstopmode");
                    compiler.working_dir = cache_subdir.clone();
                    compiler.current_dir = Some(input_dir.clone());

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
        });
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
            _ => {}
        };
    } else if args.is_present("unite") {
        info!("Pasting precompiled frames into original document!");

        let mut united_tex = format!(
            "{}\n{}",
            "\\RequirePackage{pdfpages}", parsed_file.file_content
        );
        for (f, (hash, _)) in frames.iter().zip(generated_documents) {
            let pdf = cache_subdir
                .join(format!("{:x}.pdf", hash))
                .to_string_lossy()
                .replace('\\', "/");
            united_tex = united_tex.replacen(
                f,
                &format!("{{\\setbeamercolor{{background canvas}}{{bg=}}\n\\includepdf[pages=-]{{{}}}\n}}", &pdf),
                1,
            );
        }

        let united_tex_file = input_dir.join("faster-beamer-united.tex");
        let united_pdf = cache_subdir.join("faster-beamer-united.pdf");
        let write_result = write(&united_tex_file, united_tex);
        if write_result.is_ok() {
            let mut compiler = LatexCompiler::new()
                .unwrap()
                .add_arg("-shell-escape")
                .add_arg("-interaction=nonstopmode");
            compiler.working_dir = cache_subdir;
            compiler.current_dir = Some(input_dir.clone());

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
                if let Err(err) = std::fs::remove_file(&united_tex_file) {
                    warn!(
                        "Failed to remove temporary united source {}: {}",
                        united_tex_file.display(),
                        err
                    );
                }
                publish_output_file(&united_pdf, &output_file)?;
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
            let (hash, _) = generated_documents[first_changed_frame];
            let compiled_pdf = cache_subdir.join(format!("{:x}.pdf", hash));

            if Path::new(&compiled_pdf).is_file() {
                publish_output_file(&compiled_pdf, &output_file)?;
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
