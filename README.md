# Reverse_engineered_ats_parser
Simulates what a recruiter sees after an ATS (Workday, Lever, Greenhouse) parses your resume.
Field schema and post-processing (Jaro ≥ 0.75 school/major matching, "Present" → currently-work-here,
`http`-only website filter, Lever link bucketing, upload limits) mirror the vendors' candidate-side JavaScript.
Server-side parsers are closed, so extracted values are a heuristic estimate.

## Usage

```
cargo run --release -- resume.pdf [-o report.html]
```

Accepts PDF, HTML or TXT. Writes `<name>.ats.html` with a parse-health score, findings,
per-ATS profile views, a keyword search box and the raw text layer.
