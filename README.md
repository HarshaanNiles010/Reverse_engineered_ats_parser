# Reverse_engineered_ats_parser
Simulates what a recruiter sees after an ATS (Workday, Lever, Greenhouse) parses your resume.
Field schema and post-processing (Jaro ≥ 0.75 school/major matching, "Present" → currently-work-here,
`http`-only website filter, Lever link bucketing, upload limits) mirror the vendors' candidate-side JavaScript.
Server-side parsers are closed, so extracted values are a heuristic estimate.

## Usage

```
cargo run --release -- resume.pdf [-o report.html]
cargo run --release -- serve [port]   # upload page on http://127.0.0.1:8080
```

Accepts PDF, DOCX, HTML or TXT. Writes `<name>.ats.html` with a parse-health score, findings,
per-ATS profile views, a keyword search box and the raw text layer.
`serve` shows the same report in the browser; uploads stay on your machine and are deleted after parsing.

### Resume editor

`serve` also hosts a resume editor at `http://127.0.0.1:8080/editor`:

- Build a resume from scratch or an example, or import an existing PDF/DOCX/HTML/TXT (pre-filled from the parser's output).
- Live checks for missing fields, malformed dates and locations, placeholder links and multi-role titles.
- Live preview; export DOCX, HTML or PDF (print dialog), and JSON for backup.
- One-click ATS check runs this tool's parser on the DOCX or HTML output.
- Output is single-column, without tables, text boxes, headers or footers, with standard section names, hyphen bullets, "Mon YYYY" dates and plain-ASCII punctuation.
- Several resumes are kept in the browser's local storage, with undo.

### Debug logging

Logs go to stderr, filtered by `RUST_LOG`: `RUST_LOG=ats_viewer=debug cargo run --release -- serve`
shows each parse stage under a per-request id. `trace` also logs findings and the extracted text
(resume content); lower levels log only counts, sizes and timings, never file names.

Man page: `man ./ats-viewer.1`, or install with `cp ats-viewer.1 /usr/local/share/man/man1/`.
