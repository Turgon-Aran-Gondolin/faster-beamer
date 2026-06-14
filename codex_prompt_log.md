## 2026-06-03

- Checked whether `faster-beamer` currently supports XeLaTeX or LuaLaTeX. Found that the code and README are currently tied to `pdflatex`.
- Began expanding engine support to `xelatex` and `lualatex`, with preamble precompilation disabled by default for those engines and documented override flags.
- Completed engine support for `pdflatex`, `xelatex`, and `lualatex`; verified CLI help, unit tests, and smoke builds for all three engines.
- Ran the current tests and a 3x2 compiler matrix on `test/feature-tour.tex`. `pdflatex` and `lualatex` worked with and without preamble precompilation; `xelatex --precompile-preamble` produced the error slide because XeTeX cannot dump a format with native fonts/font mappings.
- Rendered the compiler matrix to PNGs and created visual contact/diff sheets under `C:\tmp\fb-visual-compare`; valid `pdflatex`/`lualatex` on/off outputs were pixel-identical within each engine, while `xelatex --precompile-preamble` visually rendered the fallback error slide.
- Added cross-compiler visual diff sheets for precompile-on and precompile-off modes. Valid `xelatex` and `lualatex` outputs were almost identical; `pdflatex` differed modestly from both due to font/rendering differences.
- Provided command-line examples for using `xelatex` with the new `--engine=xelatex` option.
- Updated the default preamble-precompilation policy from the visual comparison: enabled by default for `pdflatex` and `lualatex`, disabled by default for `xelatex`.

## 2026-06-04

- Prepared the LaTeX engine-support changes for git commit and push, keeping unrelated generated files out of the commit.
- Tidied generated workspace artifacts: removed TeX auxiliary/preamble outputs and the temporary visual-comparison folder, while preserving ambiguous source/config scratch files.
- Investigated a real `lualatex` preamble-precompilation failure in `Changsha.tex`; LuaTeX dumped a usable `.fmt` and then failed during PDF backend shutdown, so faster-beamer now accepts that completed dump and keeps `lualatex` precompilation enabled by default.
- Provided command-line examples for running `faster-beamer` with `lualatex` and the preamble options.
- Ran an explicit `lualatex` check with `--precompile-preamble` and `-r` requested, targeting the real `Changsha.tex` deck.
- Confirmed the explicit `lualatex --precompile-preamble -r` run completed successfully and produced `C:\tmp\fb-changsha-lualatex-explicit-precompile-r.pdf`.
- Prepared the verified LuaLaTeX preamble-precompilation fix for git commit and push.
- Investigated follow-up LuaLaTeX frame-compilation failures reporting `Invalid argument` while opening TeX Live package/font files.
- Added a serial retry fallback for frame jobs that fail during parallel compilation and verified the patched local binary on `Changsha.tex` with explicit `lualatex --precompile-preamble -r -m=3`.
- Confirmed the reported `Invalid argument` abort output came from a binary without the serial retry fallback, then prepared the retry patch for commit, push, and local installation.
- Resumed after the installed retry build produced many LuaLaTeX frame failures during a full parallel verification run and began inspecting whether the retry path or logging needed further adjustment.
- Identified that `-p` without `--jobs` uses unbounded auto parallelism while `-m=3` is multi-pass, then capped LuaLaTeX auto parallelism at three jobs unless `--jobs` is explicitly supplied.
- Verified the capped LuaLaTeX auto-parallel build on `Changsha.tex` with `-p --precompile-preamble -r`, producing the expected PDF without frame failures.
- Explained the LuaLaTeX auto-parallel cap warning and how to override it with explicit `--jobs`.
- Explained the LuaLaTeX preamble dump warning, where LuaTeX reports a PDF backend shutdown error after producing a usable `.fmt`.
- Began improving compile-error reporting after feedback that the current version reports LaTeX failures poorly.
- Reworked LaTeX compile failure reporting to avoid dumping full transcripts, added concise error extraction tests, and verified the terminal output with a temporary failing Beamer fixture.

## 2026-06-07

- Adjusted frame numbering so title and TOC frames are labeled as `title`/`toc` and excluded from numeric frame counts.
- Updated compile status, retry progress, failure logs, and preview publish messages to use the adjusted frame labels.
- Added unit tests for title/TOC label handling and verified the suite with `cargo test`.

## 2026-06-08

- Refined TOC handling so only the optional front-matter TOC is labeled `toc`; later TOC-like frames follow normal numbering.
- Added tests for decks without a TOC and for section-title frames participating in the implicit frame-number sequence.
- Added input resolution that appends `.tex` when the provided input path is missing and does not already end in `.tex`.
- Updated watch-mode path comparison and added tests for bare input, existing bare files, and explicit `.tex` paths.

## 2026-06-09

- Began investigating SMEFT `lualatex` frame failures where generated frame sources start with NUL bytes (`^^@`).
- Confirmed the generated frame `.tex` sources were valid and the NUL bytes came from a corrupted cached frame `.aux` file.
- Added cleanup of stale frame sidecar files before each frame compile so serial retries do not reuse corrupted `.aux` files.
- Verified with `cargo test` and a successful patched run on `SMEFT_TFR/Notes/Slides/IMU.tex` using `-X -f -m=3 -p --engine=lualatex`.
- Began adjusting TOC frame rendering so the front-matter table-of-contents frame does not show a bottom-right frame number.
- Added TOC-only frame-number display suppression for Beamer themes that draw the footer with `\insertframenumber`.
- Added PDF completeness validation so damaged cached frame PDFs are recompiled or reported before `pdfunite`.
- Verified the updated suite with `cargo test` after adding TOC display and PDF validation regression tests.

## 2026-06-11

- Began investigating report that definitions placed between Beamer frames are not picked up by isolated frame compilation.
- Added between-frame document-context extraction for definition/setup commands so later isolated frame builds see macros, colors, counters, and similar definitions placed after earlier frames.
- Added regression tests for accumulated single-line and multi-line between-frame definitions while ignoring section commands that can trigger Beamer hook output.

## 2026-06-14

- Added automatic garbage removal for the dedicated `faster-beamer` cache directory, preserving the active deck cache, pruning stale entries older than 30 days, and throttling cleanup to once per day with a stamp file.
- Added regression tests for stale cache pruning and cleanup-stamp throttling, then verified the suite with `cargo test`.
- Explained why files accumulate in the faster-beamer cache: frame-level incremental compilation, LaTeX sidecar output, precompiled preambles, and interrupted or failed builds can leave reusable or stale artifacts behind.
- Prepared the tracked faster-beamer changes for git commit and push while leaving local untracked scratch files out of the commit.
- Discussed a latexmk-style guard process design where repeated faster-beamer calls for the same input file are redirected to one long-lived per-file daemon.
- Implemented a first guard-process path for watch mode: per-input guard metadata under the faster-beamer cache, localhost IPC rebuild requests, strict option matching, and a rebuild channel in the watcher loop.
- Investigated Windows `os error 32` stale sidecar cleanup warnings and extended retry backoff for transient file locks before reporting cleanup failure.
- Extended stale sidecar cleanup to cover static TeX dependencies from `\input` and `\include`, preserving source files while clearing cached LaTeX sidecars.
- Prepared guard-process and stale sidecar cleanup changes for git commit and push, excluding local scratch files from the commit.
