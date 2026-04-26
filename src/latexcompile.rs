//
//    This file is part of latexcompile which serves as wrapper around
//    some latex compilerand provides a basic templating scheme.
//    Copyright (C) 2018  Henrik Jürges
//
//    This program is free software: you can redistribute it and/or modify
//    it under the terms of the GNU General Public License as published by
//    the Free Software Foundation, either version 3 of the License, or
//    (at your option) any later version.
//
//    This program is distributed in the hope that it will be useful,
//    but WITHOUT ANY WARRANTY; without even the implied warranty of
//    MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//    GNU General Public License for more details.
//
//    You should have received a copy of the GNU General Public License
//    along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
//! # latexcompile
//!
//! This library provides a basic enviroment to produce a clean latex build.
//! It run the latex build within a `Tempdir`.
//!
//! It also provides a simple templating feature which can be used
//! to insert text fragements into the input files.
//!
//! ## Example
//!
//! ```
//! use std::collections::HashMap;
//! use std::fs::write;
//! use latexcompile::{LatexCompiler, LatexInput, LatexError};
//!
//! 
//!     // create the template map
//!     let mut dict = HashMap::new();
//!     dict.insert("test".into(), "Minimal".into());
//!     // provide the folder where the file for latex compiler are found
//!     let input = LatexInput::from("assets");
//!     // create a new clean compiler enviroment and the compiler wrapper
//!     let compiler = LatexCompiler::new(dict).unwrap();
//!     // run the underlying pdflatex or whatever
//!     let result = compiler.run("assets/test.tex", &input).unwrap();
//!
//!     // copy the file into the working directory
//!     let output = ::std::env::current_dir().unwrap().join("out.pdf");
//!     assert!(write(output, result).is_ok());
//!
//! ```
//!

use crate::fs_utils::{stage_directory_into, stage_file_into};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::str;
use tempfile::tempdir;

#[derive(Clone, Copy)]
pub enum BibliographyTool {
    Bibtex,
    Biber,
}

impl BibliographyTool {
    fn command_name(self) -> &'static str {
        match self {
            Self::Bibtex => "bibtex",
            Self::Biber => "biber",
        }
    }
}

#[derive(Clone, Copy)]
pub struct LatexRunOptions {
    latex_pass_count: usize,
    capture_stdout: bool,
    bibliography_tool: Option<BibliographyTool>,
}

impl LatexRunOptions {
    pub fn new() -> Self {
        Self {
            latex_pass_count: 1,
            capture_stdout: true,
            bibliography_tool: None,
        }
    }

    pub fn with_latex_pass_count(mut self, pass_count: usize) -> Self {
        self.latex_pass_count = pass_count.max(1);
        self
    }

    pub fn with_bibliography_tool(mut self, tool: Option<BibliographyTool>) -> Self {
        self.bibliography_tool = tool;
        self
    }
}

/// Specify all error cases with the fail api.
#[derive(Fail, Debug)]
pub enum LatexError {
    #[fail(display = "General failure: {}.", _0)]
    LatexError(String),
    #[fail(display = "Failed to convert input {}", _0)]
    Input(#[cause] std::io::Error),
    #[fail(display = "{}", _0)]
    Io(#[cause] std::io::Error),
}

/// result type alias idiom
type Result<T> = std::result::Result<T, LatexError>;

/// An alias for a command line
type Cmd = (String, Vec<String>);

/// The latex input provides the needed files
/// as tuple vector with name, buffer as tuple.
#[derive(Debug, PartialEq)]
pub struct LatexInput {
    input: Vec<(String, Vec<u8>)>,
}

impl LatexInput {
    pub fn new() -> LatexInput {
        LatexInput { input: vec![] }
    }

    /// Add a single file as input.
    /// ## Example
    /// ```
    /// # use latexcompile::{LatexCompiler, LatexInput, LatexError};
    /// 
    ///   let mut input = LatexInput::from("assets/main.tex");
    ///   input.add("name.tex", "test".as_bytes().to_vec());
    ///
    /// ```
    ///
    /// ## Note
    /// If the path is not a file or can't be converted to a string nothing is added and ok is returned.
    pub fn add_file(&mut self, file: PathBuf) -> Result<()> {
        if file.is_file() {
            if let Some(name) = file.to_str() {
                let content = fs::read(&file).map_err(LatexError::Input)?;
                self.input.push((name.to_string(), content));
            }
        }
        Ok(())
    }

    /// Add a whole folder as input.
    /// ## Example
    /// ```
    /// # use latexcompile::{LatexCompiler, LatexInput, LatexError};
    /// 
    ///   let mut input = LatexInput::from("assets");
    ///   input.add("name.tex", "test".as_bytes().to_vec());
    /// 
    /// ```
    /// ## Note
    /// If the path is not a folder nothing is added.
    pub fn add_folder(&mut self, folder: PathBuf) -> Result<()> {
        if folder.is_dir() {
            let paths = fs::read_dir(folder).map_err(LatexError::Input)?;

            for path in paths {
                let p = path.map_err(LatexError::Input)?.path();
                if p.is_file() {
                    self.add_file(p)?;
                } else if p.is_dir() {
                    self.add_folder(p)?;
                }
            }
        }
        Ok(())
    }

    pub fn add_file_lazy(&mut self, file: PathBuf, dest_path: &Path) -> Result<()> {
        if file.is_file() {
            stage_file_into(dest_path, &file).map_err(LatexError::Io)?;
        }
        Ok(())
    }

    pub fn add_folder_lazy(&mut self, folder: PathBuf, dest_path: &Path) -> Result<()> {
        if folder.is_dir() {
            stage_directory_into(dest_path, &folder).map_err(LatexError::Io)?;
        }
        Ok(())
    }

    pub fn from_lazy(path: &Path, dest_path: &Path) -> Result<LatexInput> {
        let mut input = LatexInput::new();
        let paths = fs::read_dir(path).map_err(LatexError::Input)?;

        for path in paths {
            let p = path.map_err(LatexError::Input)?.path();
            if p.is_file() {
                input.add_file_lazy(p, dest_path)?;
            } else if p.is_dir() {
                input.add_folder_lazy(p, dest_path)?;
            }
        }
        Ok(input)
    }
}

/// Provide a simple From conversion for &str to latex input.
/// If neither a valid file nor a folder an empty input struct is returned.
#[allow(unused_must_use)]
impl<'a> From<&'a str> for LatexInput {
    fn from(s: &'a str) -> LatexInput {
        let mut input = LatexInput::new();
        let path = PathBuf::from(s);
        if path.is_file() {
            input.add_file(path);
        } else if path.is_dir() {
            input.add_folder(path);
        }
        input
    }
}

/// The processor takes latex files as input and replaces
/// matching placeholders (e.g. ##someVar##) with the real
/// content provided as HashMap.

/// The wrapper struct around some latex compiler.
/// It provides a clean temporary enviroment for the
/// latex compilation.
/// ```
/// use std::fs::write;
/// use std::collections::HashMap;
/// use latexcompile::{LatexCompiler, LatexInput, LatexError};
///
/// 
///    let compiler = LatexCompiler::new(HashMap::new()).unwrap();
///    let input = LatexInput::from("assets");
///    let pdf = compiler.run("assets/main.tex", &input);
///    assert!(pdf.is_ok());
///
/// ```
pub struct LatexCompiler {
    pub working_dir: PathBuf,
    pub current_dir: Option<PathBuf>,
    cmd: Cmd,
}

impl LatexCompiler {
    /// Create a new latex compiler wrapper
    pub fn new() -> Result<LatexCompiler> {
        let dir = tempdir().map_err(LatexError::Io)?;
        Ok(Self::new_in(dir.path().to_path_buf()))
    }

    pub fn new_in(working_dir: PathBuf) -> LatexCompiler {
        let cmd = (
            "pdflatex".into(),
            vec!["-interaction=nonstopmode".into(), "-synctex=5".into()],
        );

        LatexCompiler {
            working_dir,
            current_dir: None,
            cmd,
        }
    }

    pub fn with_current_dir(mut self, current_dir: PathBuf) -> Self {
        self.current_dir = Some(current_dir);
        self
    }

    /// Add a new argument to the command-line.
    pub fn add_arg(mut self, cmd: &str) -> Self {
        self.cmd.1.push(cmd.into());
        self
    }

    /// build the command-line
    fn get_cmd(&self, main_file: &Path) -> Command {
        let current_dir = self.current_dir.as_ref().unwrap_or(&self.working_dir);
        let mut cmd = Command::new(&self.cmd.0);
        cmd.args(&self.cmd.1);

        if current_dir != &self.working_dir {
            cmd.arg(format!(
                "-output-directory={}",
                tex_path_arg(&self.working_dir)
            ));
        }

        cmd.arg(tex_path_arg(main_file)).current_dir(current_dir);
        cmd
    }

    fn run_latex_pass(&self, main_file: &Path) -> Result<()> {
        let output = self.get_cmd(main_file).output().map_err(LatexError::Io)?;
        let target = tex_path_arg(main_file);
        Self::check_command_output(output, &self.cmd.0, &target)
    }

    fn run_bibliography_pass(&self, tool: BibliographyTool, job_name: &str) -> Result<()> {
        let output = Command::new(tool.command_name())
            .arg(job_name)
            .current_dir(&self.working_dir)
            .output()
            .map_err(LatexError::Io)?;
        Self::check_command_output(output, tool.command_name(), job_name)
    }

    fn check_command_output(output: Output, command: &str, target: &str) -> Result<()> {
        if output.status.success() {
            return Ok(());
        }

        let stderr = str::from_utf8(&output.stderr).unwrap_or("").trim();
        let stdout = str::from_utf8(&output.stdout).unwrap_or("").trim();
        let mut err_msg = format!("{} failed for {}", command, target);

        if !stderr.is_empty() {
            err_msg.push_str(": ");
            err_msg.push_str(stderr);
        } else if !stdout.is_empty() {
            err_msg.push_str(": ");
            err_msg.push_str(stdout);
        }

        error!("{}", &err_msg);
        if !stdout.is_empty() {
            error!("{}", stdout);
        }

        Err(LatexError::LatexError(err_msg))
    }

    pub fn run(
        &self,
        main: &Path,
        _input: &LatexInput,
        options: LatexRunOptions,
    ) -> Result<PathBuf> {
        assert!(options.capture_stdout);

        let pdf = main.to_path_buf();
        let latex_pass_count = options.latex_pass_count.max(1);
        let job_name = pdf
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| LatexError::LatexError(format!("Failed to derive job name from {}", main.display())))?;

        if let Some(tool) = options.bibliography_tool {
            self.run_latex_pass(main)?;
            self.run_bibliography_pass(tool, job_name)?;
            for _ in 0..latex_pass_count.max(2) {
                self.run_latex_pass(main)?;
            }
        } else {
            for _ in 0..latex_pass_count {
                self.run_latex_pass(main)?;
            }
        }

        // get the output file
        let stem = PathBuf::from(pdf.file_stem().unwrap().to_str().unwrap());
        Ok(self.working_dir.join(stem.with_extension("pdf")))
    }
}

fn tex_path_arg(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
