[![CI](https://github.com/theHamsta/faster-beamer/actions/workflows/ci.yml/badge.svg)](https://github.com/theHamsta/faster-beamer/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/faster-beamer.svg)](https://crates.io/crates/faster-beamer)

# faster-beamer

An incremental compiler for LaTeX Beamer slides

## Motivation

Compiling Beamer slides takes too long.
I wanted to have a fast preview of my files even if the output is not 100% correct.

## What it does

It parses your input file and compiles each `frame` enviroment individually.
Compiled frames are cached and only recompiled if necessary.  
Of course, frame pages and citation will not be rendered correctly, but it should be sufficient to get an idea
how your frames will look like.

Executing the following line will let `faster-beamer` watch your tex-file for changes, compile all frames on changes and only output
the frame that was changed most recently.

```bash
faster-beamer presentation.tex --watch
```

If you want pdfunite to glue all the compiled frames together use:

```bash
faster-beamer presentation.tex --watch --pdfunite
```

We can also try to reinsert the precompiled frames into the orginal document. 
This will yield the most accurate result (including title, section pages). 

```bash
faster-beamer presentation.tex --watch --unite
```

If your slides use TikZ or other content that needs more than one LaTeX pass, add `--multi-pass` or `-m`:

```bash
faster-beamer presentation.tex --watch --multi-pass
faster-beamer presentation.tex --watch -m 3
```

Using `--multi-pass` without a count defaults to 2 LaTeX passes.

If your document uses a bibliography, choose the backend explicitly with `--bibliography` or `-b` so the compile order is correct:

```bash
faster-beamer presentation.tex --watch --bibliography bibtex
faster-beamer presentation.tex --watch -b biber
```

When bibliography processing is enabled, `faster-beamer` runs `pdflatex`, then the chosen bibliography backend, then `pdflatex` twice by default or `COUNT` times when `--multi-pass` is set.

If you suspect the cache contains stale or wrong frame PDFs, force a full rebuild of cached frames with `--force-recompile` or `-r`:

```bash
faster-beamer presentation.tex --watch --force-recompile
faster-beamer presentation.tex --watch -r
```

If you want to compile independent frame PDFs concurrently, enable parallelization with `--parallel` or `-p`:

```bash
faster-beamer presentation.tex --watch --parallel
faster-beamer presentation.tex --watch -p
```

## Requirements

- A Rust toolchain >= 3.39
- A working `pdflatex` on `PATH`
- `pdfunite` on `PATH` only if you want to use `--pdfunite`
- `bibtex` or `biber` on `PATH` when using `--bibliography`

## Windows notes

- The first supported Windows target is `x86_64-pc-windows-msvc`.
- Use TeX Live on Windows for the initial support path.
- Install the MSVC Rust toolchain and the Visual C++ Build Tools needed to compile the vendored parser sources.
- `--unite` is the more portable full-document mode on Windows.
- `--pdfunite` remains optional and will fail with an actionable error if `pdfunite` is not installed.
- If a PDF viewer keeps the output file locked, `faster-beamer` now reports that publish failure instead of relying on symlink replacement.

## Installation

```bash
cargo install --path . --force
```

## Thanks

A modified version of `https://github.com/santifa/latexcompile` is used in this project.
