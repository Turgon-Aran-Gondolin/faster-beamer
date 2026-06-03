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
