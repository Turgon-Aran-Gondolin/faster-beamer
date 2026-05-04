#[macro_use]
extern crate log;
#[macro_use]
extern crate lazy_static;

mod beamer;
mod fs_utils;
mod latexcompile;
mod parsing;
mod process_file;
mod tree_traversal;

use clap::{App, Arg};
use std::env;
use std::env::current_dir;
use std::path::Path;
use std::{thread, time};
use process_file::FasterBeamerError;

const HELP_EPILOGUE: &str = "Examples:\n  faster-beamer slides.tex\n  faster-beamer slides.tex -u\n  faster-beamer slides.tex -X -o slides.pdf\n  faster-beamer slides.tex -C=-draftmode -C=-file-line-error\n\nNotes:\n  Without -u/--tex-unite, -x/--pdfunite, or -X/--pdfunite-synctex, faster-beamer publishes only the newest frame.\n  -u/--tex-unite recompiles a temporary united TeX document and preserves SyncTeX.\n  --unite remains available as a compatibility alias.\n  -x/--pdfunite requires pdfunite on PATH and publishes no SyncTeX sidecar.\n  -X/--pdfunite-synctex keeps the pdfunite PDF and runs a temporary united TeX build to publish SyncTeX.\n  [OUTPUT] is an optional positional alias for -o, --output FILE.";

fn watch_label(input_file: &str) -> String {
    format!("Watch: monitoring {}", input_file)
}

fn main() {
    if env::var("RUST_LOG").is_err() {
        let mut builder = pretty_env_logger::formatted_builder();
        builder.parse_filters("info");
        builder.init();
    } else {
        pretty_env_logger::init();
    }

    let matches = App::new("faster-beamer")
        .version(option_env!("FASTER_BEAMER_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")))
        .author("Stephan Seitz <stephan.seitz@fau.de>")
        .about("Incremental compiler for Beamer LaTeX presentations")
        .after_help(HELP_EPILOGUE)
        .arg(
            Arg::with_name("watch")
                .short("w")
                .long("watch")
                .help("Watch the input file and rebuild when it changes"),
        )
        .arg(
            Arg::with_name("clean")
                .short("c")
                .long("clean")
                .help("Remove faster-beamer cache, auxiliary files, and stale temporary files for the input"),
        )
        .arg(
            Arg::with_name("INPUT")
                .help("Input .tex file to compile")
                .required(true)
                .index(1),
        )
        .arg(
            Arg::with_name("tex-unite")
                .short("u")
                .long("tex-unite")
                .alias("unite-synctex")
                .visible_alias("unite")
                .conflicts_with("pdfunite")
                .conflicts_with("pdfunite-synctex")
                .help("Compile a temporary united TeX document for the full deck and preserve SyncTeX"),
        )
        .arg(
            Arg::with_name("pdfunite")
                .short("x")
                .long("pdfunite")
                .conflicts_with("tex-unite")
                .conflicts_with("pdfunite-synctex")
                .help("Concatenate frame PDFs with the external pdfunite executable; fastest full-deck mode, but published SyncTeX is removed"),
        )
        .arg(
            Arg::with_name("pdfunite-synctex")
                .short("X")
                .long("pdfunite-synctex")
                .conflicts_with("tex-unite")
                .conflicts_with("pdfunite")
                .help("Concatenate frame PDFs with pdfunite, then run a temporary united TeX build to publish SyncTeX"),
        )
        .arg(
            Arg::with_name("frame-numbers")
                .short("f")
                .long("frame-numbers")
                .help("Preserve correct Beamer frame numbers. This can reduce cache reuse when frames move."),
        )
        .arg(
            Arg::with_name("tree-sitter")
                .short("t")
                .long("tree-sitter")
                .help("Use tree-sitter to parse LaTeX (instead of regexes)"),
        )
        .arg(
            Arg::with_name("multi-pass")
                .short("m")
                .long("multi-pass")
                .takes_value(true)
                .min_values(0)
                .max_values(1)
                .require_equals(true)
                .value_name("COUNT")
                .validator(|value: String| match value.parse::<usize>() {
                    Ok(count) if count > 0 => Ok(()),
                    _ => Err(String::from("COUNT must be a positive integer")),
                })
                .help("Run pdflatex COUNT times total; using the flag without COUNT defaults to 2 passes"),
        )
        .arg(
            Arg::with_name("bibliography")
                .short("b")
                .long("bibliography")
                .takes_value(true)
                .possible_values(&["bibtex", "biber"])
                .value_name("BACKEND")
                .help("Run bibliography processing as pdflatex, BACKEND, then pdflatex twice by default or COUNT times if --multi-pass is set"),
        )
        .arg(
            Arg::with_name("force-recompile")
                .short("r")
                .long("force-recompile")
                .help("Ignore cached frame PDFs and rebuild them from scratch"),
        )
        .arg(
            Arg::with_name("parallel")
                .short("p")
                .long("parallel")
                .help("Compile independent frame PDFs in parallel"),
        )
        .arg(
            Arg::with_name("jobs")
                .short("j")
                .long("jobs")
                .visible_alias("smp")
                .takes_value(true)
                .value_name("COUNT")
                .validator(|value: String| match value.parse::<usize>() {
                    Ok(count) if count > 0 => Ok(()),
                    _ => Err(String::from("COUNT must be a positive integer")),
                })
                .help("Compile with up to COUNT parallel jobs; implies parallel compilation"),
        )
        .arg(
            Arg::with_name("compiler-option")
                .short("C")
                .long("compiler-option")
                .takes_value(true)
                .multiple(true)
                .allow_hyphen_values(true)
                .value_name("OPTION")
                .help("Pass OPTION through to pdflatex for preamble, frame, and united builds; may be supplied multiple times"),
        )
        .arg(
            Arg::with_name("output")
                .short("o")
                .long("output")
                .visible_alias("out")
                .takes_value(true)
                .value_name("FILE")
                .conflicts_with("OUTPUT")
                .help("Write the output PDF to FILE"),
        )
        .arg(
            Arg::with_name("OUTPUT")
                .help("Optional positional alias for -o, --output FILE (defaults to INPUT with a .pdf extension)")
                .index(2),
        )
        .get_matches();

    let is_watch_mode = matches.is_present("watch");
    let input_file = matches.value_of("INPUT").unwrap();

    let cwd = current_dir().unwrap();
    let input_dir = Path::new(input_file)
        .parent()
        .unwrap_or(&cwd)
        .canonicalize()
        .unwrap_or_else(|_| cwd.to_owned());

    info!("Build requested: {}", input_file);
    if matches.is_present("clean") {
        let result = process_file::clean_generated_artifacts(input_file, &matches);
        if result == Err(FasterBeamerError::InputFileNotExistent) || result == Err(FasterBeamerError::IoError) {
            std::process::exit(-1);
        };
        return;
    }

    let result = process_file::process_file(input_file, &matches);
    if result == Err(FasterBeamerError::InputFileNotExistent) || result == Err(FasterBeamerError::IoError) {
        std::process::exit(-1);
    };

    if is_watch_mode {
        use hotwatch::{Event, Hotwatch};
        let matches = matches.clone();

        let mut hotwatch = Hotwatch::new().expect("Hotwatch failed to initialize.");
        hotwatch
            .watch(input_dir, move |event: Event| match event {
                Event::Write(file) | Event::NoticeRemove(file) => {
                    trace!("{:?} has changed.", file);
                    thread::sleep(time::Duration::from_millis(50));
                    let input_file = matches.value_of("INPUT").unwrap();
                    match (Path::new(&input_file).canonicalize(), file.canonicalize()) {
                        (Ok(file), Ok(changed_file)) if file == changed_file => {
                            let path_str = file.to_str().unwrap();
                            info!("Rebuild triggered: source changed at {}", &path_str);
                            let _result = process_file::process_file(path_str, &matches);
                        }
                        _ => {}
                    }
                }
                _ => {
                    trace!("{:?}", event);
                }
            })
            .expect("Failed to watch file!");
        info!("{}", watch_label(input_file));

        loop {
            thread::sleep(time::Duration::from_millis(100));
        }
    }
}
