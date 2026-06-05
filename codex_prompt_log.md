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
