//
// build.rs
// Copyright (C) 2019 stephan <stephan@stephan-ThinkPad-X300>
// Distributed under terms of the MIT license.
//
extern crate cc;

use std::env;
use std::process::Command;

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn emit_git_rerun_hint(path_arg: &str) {
    if let Some(path) = git_output(&["rev-parse", "--git-path", path_arg]) {
        println!("cargo:rerun-if-changed={}", path);
    }
}

fn emit_build_version() {
    let package_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| String::from("0.0.0"));
    let version_pre = env::var("CARGO_PKG_VERSION_PRE").unwrap_or_default();
    let release_version = package_version
        .split_once('-')
        .map(|(version, _)| version)
        .unwrap_or(&package_version);

    if !version_pre.is_empty() {
        let short_hash = git_output(&["rev-parse", "--short=10", "HEAD"]);
        let build_version = short_hash
            .map(|hash| format!("{}-{}", release_version, hash))
            .unwrap_or_else(|| package_version.clone());
        println!("cargo:rustc-env=FASTER_BEAMER_VERSION={}", build_version);
    } else {
        println!("cargo:rustc-env=FASTER_BEAMER_VERSION={}", package_version);
    }

    emit_git_rerun_hint("HEAD");
    emit_git_rerun_hint("logs/HEAD");
    emit_git_rerun_hint("packed-refs");
}

fn main() {
    emit_build_version();

    cc::Build::new()
        .include("tree-sitter-latex/src")
        .file("tree-sitter-latex/src/parser.c")
        .flag_if_supported("-w") // disable warnings
        .compile("tree-sitter-latex");

    cc::Build::new()
        .file("tree-sitter-latex/src/scanner.cc")
        .file("tree-sitter-latex/src/catcode.cc")
        .file("tree-sitter-latex/src/scanner_control_sequences.cc")
        .file("tree-sitter-latex/src/scanner_environments.cc")
        .file("tree-sitter-latex/src/scanner_keywords.cc")
        .file("tree-sitter-latex/src/scanner_names.cc")
        .flag_if_supported("-w") // disable warnings
        .cpp(true)
        .flag_if_supported("-std=c++11")
        .include("tree-sitter-latex/src")
        .include("tree-sitter-latex/src/tree_sitter")
        .compile("tree-sitter-latex-scanner");
}
