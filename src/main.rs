//! ats-viewer: simulate what an ATS recruiter sees after parsing a resume.
//! Field schema and post-processing mirror what was reverse-engineered from the
//! Workday / Lever / Greenhouse candidate-side bundles. Server parsers are closed,
//! so extraction here is a heuristic stand-in (pdf-extract ~ PDFBox/Tika-style text layer).

use regex::Regex;
use std::fmt::Write as _;
use std::{fs, path::Path, time::Instant};
use tracing::{debug, info, trace, warn};

mod web;

const MON: &str = r"(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Sept|Oct|Nov|Dec)[a-z]*\.?";
const WORKDAY_TYPES: &[&str] = &["pdf", "docx", "doc", "html", "htm", "txt"];
const FIELDS_OF_STUDY: &[&str] = &[
    "Computer Science", "Computer Engineering", "Software Engineering", "Electrical Engineering",
    "Mechanical Engineering", "Civil Engineering", "Chemical Engineering", "Industrial Engineering",
    "Information Technology", "Information Systems", "Data Science", "Artificial Intelligence",
    "Cybersecurity", "Mathematics", "Statistics", "Physics", "Chemistry", "Biology", "Biochemistry",
    "Economics", "Finance", "Accounting", "Business Administration", "Marketing", "Management",
    "Psychology", "Sociology", "Political Science", "English", "History", "Communications",
    "Nursing", "Medicine", "Law", "Education", "Graphic Design", "Architecture", "Philosophy",
];
const DEGREES: &[&str] = &[
    "Bachelor of Science", "Bachelor of Arts", "Bachelor of Engineering", "Bachelor of Commerce",
    "Bachelor of Technology", "Master of Science", "Master of Arts", "Master of Engineering",
    "Master of Business Administration", "Doctor of Philosophy", "Associate Degree", "Diploma",
    "High School Diploma",
];
const SPOKEN: &[&str] = &[
    "english", "french", "spanish", "hindi", "punjabi", "mandarin", "cantonese", "chinese", "arabic",
    "german", "portuguese", "japanese", "korean", "italian", "russian", "urdu", "tagalog",
    "vietnamese", "bengali", "tamil", "gujarati", "telugu", "persian", "farsi", "dutch", "polish", "turkish",
];
const CA_REGIONS: &[&str] = &["AB", "BC", "MB", "NB", "NL", "NS", "NT", "NU", "ON", "PE", "QC", "SK", "YT"];
const US_REGIONS: &[&str] = &[
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "FL", "GA", "HI", "ID", "IL", "IN", "IA", "KS",
    "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM", "NY",
    "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA", "WV",
    "WI", "WY", "DC",
];

fn re(s: &str) -> Regex {
    Regex::new(s).unwrap()
}
fn date_pat() -> String {
    format!(r"(?:{MON}\s+\d{{4}}|\d{{1,2}}/\d{{4}}|(?:19|20)\d{{2}})")
}
fn range_re() -> Regex {
    let d = date_pat();
    re(&format!(r"(?i)({d})\s*(?:–|—|-|to)\s*({d}|present|current|now|today)\b"))
}
fn single_date_re() -> Regex {
    re(&format!(r"(?i){}", date_pat()))
}

/// Jaro similarity, case-insensitive: port of Workday's G0 matcher (threshold 0.75).
fn jaro(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.to_lowercase().chars().collect(), b.to_lowercase().chars().collect());
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let win = (a.len().max(b.len()) / 2).saturating_sub(1);
    let (mut am, mut bm) = (vec![false; a.len()], vec![false; b.len()]);
    let mut m = 0.0;
    for i in 0..a.len() {
        for j in i.saturating_sub(win)..(i + win + 1).min(b.len()) {
            if !bm[j] && a[i] == b[j] {
                am[i] = true;
                bm[j] = true;
                m += 1.0;
                break;
            }
        }
    }
    if m == 0.0 {
        return 0.0;
    }
    let (ac, bc): (Vec<char>, Vec<char>) = (
        a.iter().zip(&am).filter(|x| *x.1).map(|x| *x.0).collect(),
        b.iter().zip(&bm).filter(|x| *x.1).map(|x| *x.0).collect(),
    );
    let t = ac.iter().zip(&bc).filter(|(x, y)| x != y).count() as f64 / 2.0;
    (m / a.len() as f64 + m / b.len() as f64 + (m - t) / m) / 3.0
}

fn best_match(s: &str, list: &[&str]) -> (String, f64) {
    list.iter()
        .map(|c| (c.to_string(), jaro(s, c)))
        .fold((String::new(), 0.0), |acc, x| if x.1 > acc.1 { x } else { acc })
}

#[derive(Default)]
struct Exp {
    title: String,
    company: String,
    location: String,
    from: String,
    raw_to: String,
    to: Option<String>,
    current: bool,
    description: Vec<String>,
}

#[derive(Default)]
struct Edu {
    school: String,
    location: String,
    degree: String,
    major: String,
    from: String,
    to: String,
}

struct Warn {
    sev: &'static str,
    msg: String,
}

// shortcut: city is 1-2 capitalized words, so "Acme San Francisco, CA" splits as company "Acme San".
fn split_location(line: &str) -> (String, String) {
    let r = re(r"^(.*\S)\s+([A-Z][a-zA-Z.]+(?: [A-Z][a-zA-Z.]+)?, [A-Z][A-Za-z]+(?: [A-Z][a-z]+)*)$");
    match r.captures(line) {
        Some(c) => (c[1].trim().into(), c[2].into()),
        None => (line.trim().into(), String::new()),
    }
}

fn is_sentence(l: &str) -> bool {
    l.split_whitespace().count() > 9
        || l.starts_with(char::is_lowercase)
        || (l.ends_with('.') && !re(r"(?i)\b(inc|ltd|co|corp|llc|plc)\.$").is_match(l))
}

fn classify_header(line: &str) -> Option<(&'static str, bool)> {
    let norm: String = line.to_lowercase().chars().map(|c| if c.is_alphabetic() { c } else { ' ' }).collect();
    let norm = norm.split_whitespace().collect::<Vec<_>>().join(" ");
    let known: &[(&str, &[&str])] = &[
        ("summary", &["summary", "professional summary", "profile", "objective", "about me", "career summary"]),
        ("skills", &["skills", "technical skills", "core skills", "core competencies", "key skills", "skills and expertise"]),
        ("experience", &["experience", "work experience", "professional experience", "employment history", "employment", "work history", "relevant experience"]),
        ("education", &["education", "academic background", "education and training"]),
        ("certifications", &["certifications", "certificates", "licenses and certifications", "certifications and licenses", "licenses"]),
        ("projects", &["projects", "personal projects", "selected projects"]),
        ("other", &["awards", "publications", "volunteer experience", "volunteering", "languages", "interests", "references", "achievements", "honors and awards"]),
    ];
    for (sec, names) in known {
        if names.contains(&norm.as_str()) {
            return Some((sec, true));
        }
    }
    let words = line.split_whitespace().count();
    // Combined headers like "Education & Certification": keyed on the first known word.
    if words <= 4 && !line.starts_with(char::is_lowercase) && !line.contains(['.', ',', ':', '@']) {
        for word in norm.split(' ') {
            if let Some((sec, _)) = known.iter().find(|(_, names)| names.contains(&word)) {
                return Some((sec, false));
            }
        }
    }
    let caps = line.chars().any(|c| c.is_alphabetic()) && !line.chars().any(|c| c.is_lowercase() || c.is_ascii_digit());
    ((1..=4).contains(&words) && caps && !line.contains('@')).then_some(("unknown", false))
}

fn parse_experience(lines: &[&str], rr: &Regex) -> Vec<Exp> {
    let loc_only = re(r"^[A-Z][a-zA-Z.]+(?: [A-Z][a-zA-Z.]+)?, [A-Z][A-Za-z]+(?: [A-Z][a-z]+)*$");
    let idx: Vec<usize> = (0..lines.len()).filter(|&i| rr.is_match(lines[i])).collect();
    let (mut out, mut starts) = (vec![], vec![]);
    for (n, &k) in idx.iter().enumerate() {
        let floor = if n == 0 { 0 } else { idx[n - 1] + 1 };
        let c = rr.captures(lines[k]).unwrap();
        let whole = c.get(0).unwrap();
        let rest = format!("{} {}", &lines[k][..whole.start()], &lines[k][whole.end()..]);
        let mut e = Exp { title: rest.trim().trim_matches(|ch: char| " |,–—-".contains(ch)).into(), ..Default::default() };
        // Header lines above the date line, nearest first, never crossing the previous job or a sentence.
        // Date alone on its line: look up to 3 lines (title, location, company); else 1 (company + location).
        let mut above = vec![];
        let mut i = k;
        while i > floor && above.len() < if e.title.is_empty() { 3 } else { 1 } && !is_sentence(lines[i - 1]) {
            i -= 1;
            above.push(lines[i]);
        }
        let mut above = above.into_iter();
        if e.title.is_empty() {
            e.title = above.next().unwrap_or_default().into();
        }
        match above.next() {
            Some(l) if loc_only.is_match(l) && above.len() > 0 => (e.location, e.company) = (l.into(), above.next().unwrap_or_default().into()),
            Some(l) => (e.company, e.location) = split_location(l),
            None => {}
        }
        if e.company.is_empty() {
            for sep in [" | ", " at ", " @ ", " - ", " – "] {
                if let Some((t, co)) = e.title.clone().split_once(sep) {
                    (e.title, e.company) = (t.trim().into(), co.trim().into());
                    break;
                }
            }
        }
        e.from = c[1].into();
        e.raw_to = c[2].into();
        let to_l = e.raw_to.to_lowercase();
        // Workday maps only "present" to "I currently work here".
        e.current = to_l == "present";
        e.to = match to_l.as_str() {
            "present" => Some("present".into()),
            "current" | "now" | "today" => None,
            _ => Some(e.raw_to.clone()),
        };
        // Header starts at the farthest line actually used; unused lookback lines stay in the previous description.
        starts.push(i + above.len());
        out.push(e);
    }
    for n in 0..out.len() {
        let end = starts.get(n + 1).copied().unwrap_or(lines.len());
        out[n].description = lines[idx[n] + 1..end.max(idx[n] + 1)].iter().map(|s| s.to_string()).collect();
    }
    out
}

fn take_dates(l: &str, rr: &Regex, dr: &Regex) -> (String, String, String) {
    if let Some(c) = rr.captures(l) {
        let m = c.get(0).unwrap();
        return (c[1].into(), c[2].into(), format!("{} {}", &l[..m.start()], &l[m.end()..]).trim().into());
    }
    if let Some(m) = dr.find(l) {
        return (String::new(), m.as_str().into(), format!("{} {}", &l[..m.start()], &l[m.end()..]).trim().into());
    }
    (String::new(), String::new(), l.into())
}

fn parse_education(lines: &[&str], rr: &Regex, dr: &Regex) -> (Vec<Edu>, Vec<String>) {
    let school_re = re(r"(?i)\b(university|college|institute|school|polytechnic|academy)\b");
    let degree_re = re(r"(?i)\b(bachelor|master|doctor|associate|diploma|b\.?\s?sc|b\.?\s?eng|b\.?\s?tech|m\.?\s?sc|mba|m\.?\s?eng|ph\.?\s?d)\b");
    let loc_only = re(r"^[A-Z][a-zA-Z.]+(?: [A-Z][a-zA-Z.]+)?, [A-Z][A-Za-z]+(?: [A-Z][a-z]+)*$");
    let cert_re = re(r"(?i)(certif|comptia|security\+|aws certified|pmp|cissp|scrum master|\(\w{2,4}-\d{3}\))");
    let (mut out, mut stray) = (Vec::<Edu>::new(), vec![]);
    for l in lines {
        let (from, to, rest) = take_dates(l, rr, dr);
        if let Some(m) = degree_re.find(&rest) {
            if out.is_empty() {
                out.push(Edu::default());
            }
            let e = out.last_mut().unwrap();
            let tail = &rest[m.start()..];
            let full = ["bachelor", "master", "doctor", "associate", "diploma"].iter().any(|w| m.as_str().to_lowercase().starts_with(w));
            let (d, mj) = if full {
                tail.split_once(',').or_else(|| tail.split_once(" in ")).unwrap_or((tail, ""))
            } else {
                // "B.Sc. Computer Science, Minor in X": abbreviation, then major up to the comma.
                (m.as_str(), tail[m.len()..].trim_start_matches('.').split(',').next().unwrap_or(""))
            };
            (e.degree, e.major) = (d.trim().into(), mj.trim().into());
            if !to.is_empty() {
                (e.from, e.to) = (from, to);
            }
        } else if school_re.is_match(&rest) {
            let (s, loc) = split_location(&rest);
            out.push(Edu { school: s, location: loc, from, to, ..Default::default() });
        } else if cert_re.is_match(&rest) {
            stray.push(rest);
        } else if let Some(e) = out.last_mut() {
            // One-item-per-line layouts: a bare location or bare date line belongs to the school above.
            if rest.is_empty() && e.to.is_empty() {
                (e.from, e.to) = (from, to);
            } else if e.location.is_empty() && loc_only.is_match(&rest) {
                e.location = rest;
            }
        }
    }
    (out, stray)
}

/// Split at commas/semicolons/pipes/bullets, but not inside parentheses: "AWS (EC2, S3)" stays one skill.
fn split_skills(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth) = (vec![], String::new(), 0i32);
    for ch in s.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth <= 0 && ",;|•".contains(ch) {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(ch);
        }
    }
    out.push(cur);
    out.into_iter().map(|x| x.trim().trim_end_matches('.').to_string()).filter(|x| !x.is_empty()).collect()
}

fn parse_skills(lines: &[&str]) -> Vec<(String, Vec<String>)> {
    let label = re(r"^([A-Za-z&/ ]{2,40}):\s*(.*)$");
    let mut groups: Vec<(String, String)> = vec![];
    for l in lines {
        match (label.captures(l), groups.last_mut()) {
            (Some(c), _) => groups.push((c[1].into(), c[2].into())),
            (None, Some(g)) => {
                g.1.push(' ');
                g.1.push_str(l)
            }
            (None, None) => groups.push((String::new(), l.to_string())),
        }
    }
    groups.into_iter().map(|(g, t)| (g, split_skills(&t))).collect()
}

fn char_issue(c: char) -> Option<(&'static str, &'static str)> {
    Some(match c {
        '\u{FFFD}' => ("high", "Replacement character � (glyph lost in extraction)"),
        '\u{E000}'..='\u{F8FF}' => ("high", "Private-use glyph (icon font or undecodable text)"),
        '\u{FB00}'..='\u{FB06}' => ("high", "Ligature glyph (ﬁ/ﬂ): 'ﬁle' will not match a search for 'file'"),
        '→' | '✓' | '✔' | '◆' | '▪' | '●' | '•' | '►' | '★' | '➤' | '■' | '◦' => ("med", "Symbol / fancy bullet"),
        '‘' | '’' | '“' | '”' => ("low", "Smart quote (fine in UTF-8 parsers, risky in legacy ones)"),
        '–' | '—' => ("low", "En/em dash (fine in UTF-8 parsers, risky in legacy ones)"),
        c if c.is_control() && !"\n\r\t".contains(c) => ("high", "Control character"),
        c if !c.is_ascii() => ("low", "Other non-ASCII character"),
        _ => return None,
    })
}

fn extract(path: &Path) -> Result<String, String> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    match ext.as_str() {
        "pdf" => {
            let p = path.to_path_buf();
            // pdf-extract panics on some malformed PDFs; treat that as an extraction failure.
            std::panic::catch_unwind(move || pdf_extract::extract_text(&p))
                .map_err(|_| {
                    warn!("pdf-extract panicked");
                    "PDF parser crashed on this file (an ATS parser may fail too)".to_string()
                })?
                .map_err(|e| format!("PDF text extraction failed: {e}"))
        }
        "txt" => fs::read_to_string(path).map_err(|e| e.to_string()),
        "html" | "htm" => {
            let h = fs::read_to_string(path).map_err(|e| e.to_string())?;
            let h = re(r"(?is)<(head|script|style)[^>]*>.*?</(head|script|style)>").replace_all(&h, "");
            let h = re(r"(?i)<br\s*/?>|</(p|div|li|h\d|tr|section|header)>").replace_all(&h, "\n");
            let h = re(r"<[^>]+>").replace_all(&h, " ");
            Ok(decode_entities(&h))
        }
        "docx" => read_docx(path),
        _ => Err(format!("'.{ext}' not supported by this viewer; use PDF, DOCX, HTML or TXT")),
    }
}

fn decode_entities(h: &str) -> String {
    let named = [("nbsp", " "), ("lt", "<"), ("gt", ">"), ("quot", "\""), ("apos", "'"), ("ndash", "–"), ("mdash", "—"), ("rarr", "→"), ("middot", "·"), ("bull", "•"), ("hellip", "…"), ("rsquo", "’"), ("lsquo", "‘"), ("ldquo", "“"), ("rdquo", "”")];
    let h = re(r"&(#x[0-9a-fA-F]+|#\d+|[a-z]+);").replace_all(h, |c: &regex::Captures| {
        let e = &c[1];
        let num = e.strip_prefix("#x").map(|x| u32::from_str_radix(x, 16).ok()).unwrap_or_else(|| e.strip_prefix('#').and_then(|d| d.parse().ok()));
        match num {
            Some(n) => char::from_u32(n).map(String::from).unwrap_or_default(),
            None => named.iter().find(|x| x.0 == e).map(|x| x.1.to_string()).unwrap_or_else(|| c[0].to_string()),
        }
    });
    // &amp; last so "&amp;lt;" stays literal.
    h.replace("&amp;", "&")
}

/// Text of one WordprocessingML part: <w:t> runs, paragraph ends and breaks as newlines, tabs as tabs.
fn docx_xml_text(xml: &str) -> String {
    // Drop VML fallbacks: they duplicate text-box content already in mc:Choice.
    let xml = re(r"(?s)<mc:Fallback>.*?</mc:Fallback>").replace_all(xml, "");
    let tok = re(r"<w:t(?:\s[^>]*)?>([^<]*)</w:t>|(</w:p>|<w:br\b[^>]*/>|<w:cr/>)|(<w:tab/>)");
    let mut out = String::new();
    for c in tok.captures_iter(&xml) {
        match (c.get(1), c.get(2)) {
            (Some(t), _) => out.push_str(&decode_entities(t.as_str())),
            (_, Some(_)) => out.push('\n'),
            _ => out.push('\t'),
        }
    }
    out
}

/// Reads DOCX like Tika does: headers first, then body, then footers. Container findings are
/// returned as text lines prefixed "\0ats:" so main() can turn them into warnings.
fn read_docx(path: &Path) -> Result<String, String> {
    use std::io::Read;
    const MAX_PART: u64 = 20 * 1024 * 1024; // zip-bomb guard per XML part
    let f = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(f).map_err(|e| format!("not a valid DOCX (zip) file: {e}"))?;
    let names: Vec<String> = zip.file_names().filter_map(|n| n.ok().map(String::from)).collect();
    let mut read = |name: &str| -> Result<String, String> {
        let mut s = String::new();
        zip.by_name(name).map_err(|e| e.to_string())?.take(MAX_PART).read_to_string(&mut s).map_err(|e| format!("{name}: {e}"))?;
        Ok(s)
    };
    let body = read("word/document.xml")?;
    let mut parts = |prefix: &str| -> Result<String, String> {
        let mut v: Vec<&String> = names.iter().filter(|n| n.starts_with(prefix) && n.ends_with(".xml")).collect();
        v.sort();
        v.into_iter().map(|n| read(n).map(|x| docx_xml_text(&x))).collect::<Result<Vec<_>, _>>().map(|t| t.join("\n"))
    };
    let (headers, footers) = (parts("word/header")?, parts("word/footer")?);
    debug!(parts = names.len(), body_bytes = body.len(), header_chars = headers.len(), footer_chars = footers.len(), "docx read");
    let mut notes = vec![];
    if names.iter().any(|n| n.ends_with("vbaProject.bin")) {
        notes.push("high|Contains VBA macros: Workday's upload scan flags containsMacros and blocks the file.");
    }
    if body.contains("<w:tbl>") || body.contains("<w:tbl ") {
        notes.push("med|Uses tables: cells are read one after another, so side-by-side columns interleave.");
    }
    if body.contains("w:txbxContent") {
        notes.push("med|Uses text boxes: some parsers skip them or place their text out of order.");
    }
    if !(headers.trim().is_empty() && footers.trim().is_empty()) {
        notes.push("med|Text in Word header/footer: some parsers drop it, others merge it into the body.");
    }
    let notes: String = notes.iter().map(|n| format!("\0ats:{n}\n")).collect();
    Ok(format!("{notes}{headers}\n{}\n{footers}", docx_xml_text(&body)))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Tokenizer approximating search indexing: lowercase, punctuation becomes spaces (keeps + and #).
fn normalize(s: &str) -> String {
    let t: String = s.to_lowercase().chars().map(|c| if c.is_alphanumeric() || c == '+' || c == '#' { c } else { ' ' }).collect();
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // RUST_LOG overrides, e.g. RUST_LOG=ats_viewer=debug. Defaults: info for serve, warnings only for the CLI.
    let default = if args.get(1).map(String::as_str) == Some("serve") { "ats_viewer=info,tower_http=info" } else { "warn" };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| default.into()))
        .with_writer(std::io::stderr)
        .init();
    if args.get(1).map(String::as_str) == Some("serve") {
        let port = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(8080);
        return web::serve(port);
    }
    let Some(input) = args.get(1) else {
        eprintln!("usage: ats-viewer <resume.pdf|.docx|.html|.txt> [-o report.html]\n       ats-viewer serve [port]");
        std::process::exit(2);
    };
    let path = Path::new(input);
    let out_path = match args.iter().position(|a| a == "-o") {
        Some(i) => args.get(i + 1).cloned().unwrap_or_else(|| "report.html".into()),
        None => format!("{}.ats.html", path.file_stem().and_then(|s| s.to_str()).unwrap_or("resume")),
    };
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (html, summary, _) = analyze(path, &name).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    if let Err(e) = fs::write(&out_path, html) {
        eprintln!("cannot write {out_path}: {e}");
        std::process::exit(1);
    }
    print!("{summary}");
    println!("Report: {out_path}");
}

/// Parses one resume file. Returns the HTML report, a plain-text summary, and the parsed fields
/// as JSON (the editor's import format).
/// `display_name` is shown in the report instead of the on-disk path (uploads use temp files).
/// Logs carry counts, sizes and timings only; resume text appears only at trace level.
#[tracing::instrument(level = "debug", skip_all, err(level = "warn"))]
fn analyze(path: &Path, display_name: &str) -> Result<(String, String, String), String> {
    let t0 = Instant::now();
    let size = fs::metadata(path).map_err(|e| format!("cannot read {display_name}: {e}"))?.len();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    debug!(%ext, bytes = size, "ingest");
    let mut warns: Vec<Warn> = vec![];
    let mut w = |sev: &'static str, msg: String| warns.push(Warn { sev, msg });

    // 1. Ingest checks (limits taken from the candidate-side bundles).
    let mb = size as f64 / 1_048_576.0;
    let ingest = [
        ("Workday", WORKDAY_TYPES.contains(&ext.as_str()) && size <= 5 * 1024 * 1024, "PDF/DOC/DOCX/HTML/TXT, ≤ 5 MB, virus + macro scan"),
        ("Ashby", size <= 6 * 1024 * 1024, "≤ 6 MB, reCAPTCHA before autofill"),
        ("Lever", size <= 100_000_000, "≤ 100 MB, hCaptcha on page"),
    ];
    for (name, ok, rule) in &ingest {
        if !ok {
            w("high", format!("{name} would reject this file at upload ({rule})."));
        }
    }

    // 2. Text extraction.
    let text = extract(path)?;
    let text: String = text
        .split_inclusive('\n')
        .filter(|l| match l.strip_prefix("\0ats:").and_then(|n| n.trim_end().split_once('|')) {
            Some((sev, msg)) => {
                w(if sev == "high" { "high" } else { "med" }, msg.to_string());
                false
            }
            None => true,
        })
        .collect();
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    debug!(chars = text.chars().count(), lines = lines.len(), words = text.split_whitespace().count(), ms = t0.elapsed().as_millis() as u64, "text extracted");
    trace!(%text, "raw text layer");
    if text.split_whitespace().count() < 50 {
        w("high", "Almost no text layer. Image-only PDF (Canva/scan): every ATS field will be empty.".into());
    }

    // 3. Character / encoding scan.
    let chars: Vec<char> = text.chars().collect();
    let mut issues: Vec<(&str, &str, usize, Vec<String>)> = vec![];
    for (i, &c) in chars.iter().enumerate() {
        if let Some((sev, label)) = char_issue(c) {
            let pos = match issues.iter().position(|x| x.1 == label) {
                Some(p) => p,
                None => {
                    issues.push((sev, label, 0, vec![]));
                    issues.len() - 1
                }
            };
            issues[pos].2 += 1;
            if issues[pos].3.len() < 3 {
                let ctx: String = chars[i.saturating_sub(25)..(i + 25).min(chars.len())].iter().collect();
                issues[pos].3.push(ctx.replace('\n', " ⏎ "));
            }
        }
    }
    let broken: Vec<String> = re(r"\b([B-HJ-Z]) ([a-z]{3,})\b").find_iter(&text).map(|m| m.as_str().to_string()).collect();
    let mut broken = broken;
    broken.sort();
    broken.dedup();
    broken.truncate(5);
    if !broken.is_empty() {
        w("high", format!("Words split by stray spaces in text layer: {}. Searches for these words miss.", broken.join(", ")));
    }

    // 4. Header/footer leakage (browser print headers, file paths, page counters).
    let hf_re = re(r"^\d{1,2}/\d{1,2}/\d{2,4},\s*\d{1,2}:\d{2}|(?:file|ﬁle):///|^Page \d+ of \d+$");
    let hf_lines: Vec<&str> = lines.iter().copied().filter(|l| hf_re.is_match(l)).collect();
    if !hf_lines.is_empty() {
        w("high", format!("{} header/footer line(s) in the text layer, e.g. \"{}\". They land mid-section and can leak local file paths. Re-print with \"Headers and footers\" off.", hf_lines.len(), hf_lines[0]));
    }

    // 5. Sections.
    let mut sections: Vec<(&str, String, Vec<&str>)> = vec![("contact", String::new(), vec![])];
    for l in &lines {
        match classify_header(l) {
            Some((sec, std_)) => {
                if !std_ && sec == "unknown" {
                    w("med", format!("Non-standard section header \"{l}\": parser will not know what it holds."));
                } else if !std_ {
                    w("low", format!("Combined header \"{l}\" read as \"{sec}\"; everything under it is parsed as {sec}."));
                }
                sections.push((sec, l.to_string(), vec![]));
            }
            None => sections.last_mut().unwrap().2.push(l),
        }
    }
    let sec = |name: &str| -> Vec<&str> { sections.iter().filter(|s| s.0 == name).flat_map(|s| s.2.clone()).collect() };
    for need in ["experience", "education", "skills"] {
        if !sections.iter().any(|s| s.0 == need) {
            w("med", format!("No recognizable \"{need}\" section header."));
        }
    }

    // 6. Contact block.
    let contact = sec("contact");
    let name = contact.first().filter(|l| {
        let n = l.split_whitespace().count();
        (2..=4).contains(&n) && l.chars().all(|c| c.is_alphabetic() || " -'.".contains(c))
    });
    let blob = contact.join(" | ");
    let email = re(r"[\w.+-]+@[\w-]+(?:\.[\w-]+)+").find(&blob).map(|m| m.as_str().to_string());
    let phone = re(r"(\+\d{1,3}[\s.-]?)?\(?\d{3}\)?[\s.-]?\d{3}[\s.-]?\d{4}").find(&blob).map(|m| m.as_str().to_string());
    let no_email = re(r"[\w.+-]+@[\w-]+(?:\.[\w-]+)+").replace_all(&blob, " ");
    let urls: Vec<String> = re(r"(?i)\b(?:https?://)?(?:www\.)?(?:[a-z0-9-]+\.)+(?:com|ca|io|dev|org|net|me|ai|app|co)(?:/[^\s|,]*)?")
        .find_iter(&no_email)
        .map(|m| m.as_str().to_string())
        .collect();
    let loc_re = re(r"^([A-Z][A-Za-z .'-]+), ([A-Z]{2}|[A-Z][a-z]+(?: [A-Z][a-z]+)*)$");
    let location = blob.split(['|', '•', '·']).map(str::trim).find_map(|s| loc_re.captures(s).map(|c| (c[1].to_string(), c[2].to_string())));
    if name.is_none() {
        w("high", "No name detected on the first line.".into());
    }
    if email.is_none() {
        w("high", "No email detected in the header block.".into());
    }
    if phone.is_none() {
        w("med", "No phone number detected.".into());
    }
    for u in &urls {
        if re(r"(?i)replace|your-?name|xxx|example").is_match(u) {
            w("high", format!("Placeholder URL left in resume: {u}"));
        }
    }

    debug!(sections = ?sections.iter().map(|s| s.0).collect::<Vec<_>>(), "sections");

    // 7. Body sections.
    let (rr, dr) = (range_re(), single_date_re());
    let exps = parse_experience(&sec("experience"), &rr);
    let (edus, stray_certs) = parse_education(&sec("education"), &rr, &dr);
    let skills = parse_skills(&sec("skills"));
    let certs: Vec<String> = sec("certifications").iter().map(|l| take_dates(l, &rr, &dr).2).collect();
    let flat_skills: Vec<&String> = skills.iter().flat_map(|g| &g.1).collect();
    let languages: Vec<&&String> = flat_skills.iter().filter(|s| SPOKEN.contains(&s.to_lowercase().as_str())).collect();
    debug!(jobs = exps.len(), schools = edus.len(), skills = flat_skills.len(), certs = certs.len(), stray_certs = stray_certs.len(), urls = urls.len(), "parsed");

    for e in &exps {
        if e.company.is_empty() {
            w("high", format!("Job \"{}\": no company found. Recruiter searches by employer miss it.", e.title));
        }
        if e.title.is_empty() {
            w("high", format!("Job at \"{}\": no title found.", e.company));
        }
        if e.title.chars().any(|c| "→|/".contains(c)) {
            w("med", format!("Title \"{}\" holds several roles/symbols; the ATS stores it as one title string.", e.title));
        }
        if e.to.is_none() {
            w("med", format!("\"{}\" end date \"{}\": only \"Present\" maps to \"I currently work here\" in Workday.", e.title, e.raw_to));
        }
        if let Some(h) = e.description.iter().find(|d| hf_re.is_match(d)) {
            w("high", format!("Page header/footer text \"{h}\" got merged into the \"{}\" description.", e.title));
        }
    }
    if exps.is_empty() && sections.iter().any(|s| s.0 == "experience") {
        w("high", "Experience section found but no date ranges parsed; jobs will be missing.".into());
    }
    for c in &stray_certs {
        w("med", format!("Certification \"{c}\" sits under Education; parsers may store it as a school or drop it. Give it a CERTIFICATIONS section."));
    }
    if languages.is_empty() {
        let label = if skills.iter().any(|g| g.0.eq_ignore_ascii_case("languages")) { " (your \"Languages:\" label lists programming languages)" } else { "" };
        w("low", format!("No spoken languages in skills{label}; Workday's Languages field stays empty."));
    }
    let mut kinds: Vec<&str> = exps.iter().flat_map(|e| [e.from.as_str()]).chain(edus.iter().map(|e| e.to.as_str())).filter(|d| !d.is_empty())
        .map(|d| if d.contains('/') { "MM/YYYY" } else if d.chars().any(char::is_alphabetic) { "Mon YYYY" } else { "YYYY" }).collect();
    kinds.sort();
    kinds.dedup();
    if kinds.len() > 1 {
        w("low", format!("Mixed date formats: {}.", kinds.join(", ")));
    }
    for (sev, label, n, _) in &issues {
        if *sev != "low" {
            w(sev, format!("{n}× {label}"));
        }
    }

    // 8. Heuristic confidence score (vendor scores are not visible; this is ours).
    let score = warns.iter().fold(100i32, |s, x| s - match x.sev { "high" => 12, "med" => 5, _ => 1 }).max(0);
    warns.sort_by_key(|x| match x.sev { "high" => 0, "med" => 1, _ => 2 });
    for x in &warns {
        trace!(sev = x.sev, msg = %x.msg, "finding");
    }
    let count = |sev: &str| warns.iter().filter(|x| x.sev == sev).count();
    info!(%ext, bytes = size, score, high = count("high"), med = count("med"), low = count("low"), ms = t0.elapsed().as_millis() as u64, "analyzed");

    // ---------- HTML ----------
    let mut h = String::new();
    let row = |h: &mut String, k: &str, v: &str, note: &str| {
        let v = if v.is_empty() { "<span class=empty>(empty)</span>".to_string() } else { esc(v) };
        let _ = write!(h, "<tr><th>{}</th><td>{v}</td><td class=note>{note}</td></tr>", esc(k));
    };
    let (first, middle, last) = match name.map(|n| n.split_whitespace().collect::<Vec<_>>()) {
        Some(p) => (p[0].to_string(), p[1..p.len() - 1].join(" "), p[p.len() - 1].to_string()),
        None => Default::default(),
    };
    let (city, region) = location.clone().unwrap_or_default();
    let country = if CA_REGIONS.contains(&region.as_str()) { "CAN" } else if US_REGIONS.contains(&region.as_str()) { "USA" } else { "" };
    let _ = write!(h, r##"<!doctype html><html><head><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1"><title>ATS View: {fname}</title><style>
:root{{--bg:#f6f7f9;--card:#fff;--fg:#1d2330;--mut:#667085;--line:#e3e6ea;--hi:#c0362c;--med:#b7791f;--low:#667085;--ok:#2f855a;--acc:#2456c7}}
@media (prefers-color-scheme:dark){{:root{{--bg:#12151b;--card:#1b1f27;--fg:#e6e8ec;--mut:#98a2b3;--line:#2c323d;--hi:#f0706a;--med:#e0b050;--low:#98a2b3;--ok:#5fc48a;--acc:#7aa2ff}}}}
body{{margin:0;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif}}main{{max-width:1000px;margin:auto;padding:16px}}
h1{{font-size:22px;margin:8px 0}}h2{{font-size:17px;margin:0 0 10px}}h3{{font-size:14px;margin:14px 0 6px;color:var(--mut);text-transform:uppercase;letter-spacing:.04em}}
.card{{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:16px;margin:14px 0;overflow-x:auto}}
table{{border-collapse:collapse;width:100%}}th,td{{text-align:left;padding:6px 8px;border-bottom:1px solid var(--line);vertical-align:top}}th{{width:28%;color:var(--mut);font-weight:500}}
.note{{color:var(--mut);font-size:13px;width:30%}}.empty{{color:var(--hi);font-style:italic}}.high{{color:var(--hi)}}.med{{color:var(--med)}}.low{{color:var(--low)}}.ok{{color:var(--ok)}}
.score{{font-size:40px;font-weight:700}}pre{{white-space:pre-wrap;font:13px/1.45 ui-monospace,monospace;margin:0}}.job{{border-left:3px solid var(--acc);padding:4px 12px;margin:10px 0}}
.chip{{display:inline-block;border:1px solid var(--line);border-radius:12px;padding:1px 9px;margin:2px;font-size:13px}}input{{font:inherit;padding:6px 10px;width:100%;box-sizing:border-box;border:1px solid var(--line);border-radius:6px;background:var(--bg);color:var(--fg)}}
nav a{{margin-right:14px;color:var(--acc)}}</style></head><body><main>
<h1>What the recruiter sees</h1><p class=note>{fname} · {mb:.2} MB · simulated from reverse-engineered candidate-side code. Server parsers are closed, so field values are a heuristic estimate. Recruiters see these fields unless the candidate corrects them before submitting.</p>
<nav><a href="#score">Score</a><a href="#workday">Workday</a><a href="#lever">Lever</a><a href="#greenhouse">Greenhouse</a><a href="#search">Keyword search</a><a href="#raw">Raw text</a></nav>
<div class=card id=score><h2>Parse health</h2><div class="score {sc}">{score}/100</div><p class=note>Heuristic: high −12, medium −5, low −1. Not a vendor score.</p><table>"##,
        fname = esc(display_name),
        sc = if score >= 80 { "ok" } else if score >= 50 { "med" } else { "high" });
    for (name, ok, rule) in &ingest {
        let _ = write!(h, "<tr><th>{name} upload</th><td class={}>{}</td><td class=note>{rule}</td></tr>", if *ok { "ok" } else { "high" }, if *ok { "accepted" } else { "rejected" });
    }
    h.push_str("</table><h3>Findings</h3><table>");
    for x in &warns {
        let _ = write!(h, "<tr><td class={0} style=width:70px>{0}</td><td>{1}</td></tr>", x.sev, esc(&x.msg));
    }
    if warns.is_empty() {
        h.push_str("<tr><td class=ok>No issues found.</td></tr>");
    }
    h.push_str("</table><h3>Character scan</h3><table>");
    for (sev, label, n, samples) in &issues {
        let _ = write!(h, "<tr><th class={sev}>{n}× {}</th><td colspan=2><pre>{}</pre></td></tr>", esc(label), esc(&samples.join("\n")));
    }
    h.push_str("</table></div>");

    // Workday
    h.push_str("<div class=card id=workday><h2>Workday: candidate profile</h2><h3>My Information</h3><table>");
    row(&mut h, "First name", &first, "");
    row(&mut h, "Middle name(s)", &middle, "everything between first and last word");
    row(&mut h, "Last name", &last, "");
    row(&mut h, "Email", email.as_deref().unwrap_or(""), "contact.emails[0]");
    let pnote = match (&phone, country) {
        (Some(p), _) if p.starts_with('+') => "country code from number".to_string(),
        (Some(_), "") => "no +code, no country: falls back to posting location / browser language".into(),
        (Some(_), c) => format!("no +code: country phone code inferred from {c}"),
        _ => String::new(),
    };
    row(&mut h, "Phone", phone.as_deref().unwrap_or(""), &pnote);
    row(&mut h, "City", &city, "");
    row(&mut h, "State / Province", &region, if region.is_empty() { "" } else if country.is_empty() { "no region-code match: left blank" } else { "matched to region code" });
    row(&mut h, "Country", country, if country.is_empty() { "unknown: Workday uses profile, then posting location, then IP/browser language" } else { "from resume" });
    for u in &urls {
        let kind = ["linkedin", "facebook", "twitter"].into_iter().find(|d| u.to_lowercase().contains(d)).unwrap_or("website");
        let note = if u.starts_with("http") { "kept" } else { "dropped by client filter /http.*/ unless the parser adds a scheme" };
        row(&mut h, kind, u, note);
    }
    h.push_str("</table><h3>Work Experience</h3>");
    for e in &exps {
        let to = match &e.to {
            Some(t) if t == "present" => "Present ☑ I currently work here".to_string(),
            Some(t) => t.clone(),
            None => format!("(empty; \"{}\" not understood)", e.raw_to),
        };
        let _ = write!(h, "<div class=job><table>");
        row(&mut h, "Job title", &e.title, "");
        row(&mut h, "Company", &e.company, "");
        row(&mut h, "Location", &e.location, "");
        row(&mut h, "From", &e.from, "");
        row(&mut h, "To", &to, "");
        let _ = write!(h, "<tr><th>Role description</th><td colspan=2><pre>{}</pre></td></tr></table></div>", esc(&e.description.join("\n")));
    }
    if exps.is_empty() {
        h.push_str("<p class=empty>(no jobs parsed)</p>");
    }
    h.push_str("<h3>Education</h3>");
    for e in &edus {
        let (dm, ds) = best_match(&e.degree, DEGREES);
        let (fm, fs_) = best_match(&e.major, FIELDS_OF_STUDY);
        let _ = write!(h, "<div class=job><table>");
        row(&mut h, "School", &e.school, "fuzzy-matched to Workday's school list (Jaro ≥ 0.75); abbreviations like \"UofA\" fail");
        row(&mut h, "Location", &e.location, "");
        row(&mut h, "Degree", if ds >= 0.75 { &dm } else { "" }, &format!("parsed \"{}\" → best \"{dm}\" ({ds:.2}); approximate, Workday's degree matcher not fully seen", e.degree));
        row(&mut h, "Field of study", if fs_ >= 0.75 { &fm } else { "" }, &format!("parsed \"{}\" → \"{fm}\" Jaro {fs_:.2} {}", e.major, if fs_ >= 0.75 { "✓" } else { "✗ below 0.75, left blank" }));
        row(&mut h, "From", &e.from, "");
        row(&mut h, "To (actual or expected)", &e.to, "");
        h.push_str("</table></div>");
    }
    if edus.is_empty() {
        h.push_str("<p class=empty>(no education parsed)</p>");
    }
    h.push_str("<h3>Certifications</h3>");
    for c in &certs {
        let _ = write!(h, "<span class=chip>{}</span>", esc(c));
    }
    if certs.is_empty() {
        h.push_str("<p class=empty>(none: no CERTIFICATIONS section)</p>");
    }
    h.push_str("<h3>Languages</h3>");
    for l in &languages {
        let _ = write!(h, "<span class=chip>{}</span>", esc(l));
    }
    if languages.is_empty() {
        h.push_str("<p class=empty>(none: only skills exactly named after a spoken language land here)</p>");
    }
    h.push_str("<h3>Skills</h3>");
    for s in &flat_skills {
        let _ = write!(h, "<span class=chip>{}</span>", esc(s));
    }
    let _ = write!(h, "<p class=note>{} skills. Workday also sends the whole parse to a skill-suggestion service, so extra suggested skills may appear.</p></div>", flat_skills.len());

    // Lever
    h.push_str("<div class=card id=lever><h2>Lever: autofilled application</h2><table>");
    row(&mut h, "Full name", name.copied().unwrap_or(""), "");
    row(&mut h, "Email", email.as_deref().unwrap_or(""), "");
    row(&mut h, "Phone", phone.as_deref().unwrap_or(""), "");
    row(&mut h, "Current company", exps.first().map(|e| e.company.as_str()).unwrap_or(""), "most recent employer");
    row(&mut h, "Current location", &location.as_ref().map(|(c, r)| format!("{c}, {r}")).unwrap_or_default(), "");
    let mut other: Vec<&String> = vec![];
    for u in &urls {
        let l = u.to_lowercase();
        match ["linkedin", "twitter", "x.com", "github", "quora"].into_iter().find(|d| l.contains(d)) {
            Some(d) => row(&mut h, &format!("{} URL", if d == "x.com" { "twitter" } else { d }), u, ""),
            None => other.push(u),
        }
    }
    for (i, u) in other.iter().enumerate() {
        row(&mut h, "Other website", u, if i + 1 < other.len() { "overwritten: only the last 'other' link survives" } else { "" });
    }
    h.push_str("</table><p class=note>Lever's form skips any field the candidate already edited.</p></div>");

    // Greenhouse
    h.push_str("<div class=card id=greenhouse><h2>Greenhouse: no parse at apply time</h2><p>Fields are typed by the candidate (or filled from a saved MyGreenhouse / Seek profile). The recruiter sees those typed fields, the attached file, and the extracted text below. Keyword search runs on that text.</p></div>");

    // Search + raw
    let norm = normalize(&text);
    let _ = write!(h, r##"<div class=card id=search><h2>Recruiter keyword search</h2><p class=note>Exact-match search on normalized text: lowercase, punctuation → space. No stemming or synonyms.</p>
<input id=q placeholder="try: python, cross-functional, file"><p id=res class=note></p></div>
<div class=card id=raw><h2>Raw text layer (what the parser actually reads)</h2><pre>{}</pre></div></main>
<script>const T=" {} ";const n=s=>s.toLowerCase().replace(/[^\p{{L}}\p{{N}}+#]+/gu," ").trim();
q.oninput=()=>{{const k=n(q.value);if(!k){{res.textContent="";return}}const c=T.split(" "+k+" ").length-1;res.innerHTML=c?`<span class=ok>${{c}} hit(s)</span> for "${{k}}"`:`<span class=high>no match</span> for "${{k}}"`}}</script></body></html>"##,
        esc(&text), norm);

    let mut summary = format!("Parse health {score}/100 · {} jobs · {} schools · {} skills · {} certs\n", exps.len(), edus.len(), flat_skills.len(), certs.len());
    for x in warns.iter().filter(|x| x.sev != "low").take(8) {
        let _ = writeln!(summary, "  [{}] {}", x.sev, x.msg);
    }
    let list = |v: Vec<String>| format!("[{}]", v.join(","));
    let strs = |v: &[String]| list(v.iter().map(|x| jstr(x)).collect());
    let json = format!(
        r#"{{"name":{},"email":{},"phone":{},"location":{},"links":{},"summary":{},"experience":{},"education":{},"skills":{},"certifications":{},"languages":{}}}"#,
        jstr(name.copied().unwrap_or("")),
        jstr(email.as_deref().unwrap_or("")),
        jstr(phone.as_deref().unwrap_or("")),
        jstr(&location.as_ref().map(|(c, r)| format!("{c}, {r}")).unwrap_or_default()),
        strs(&urls),
        jstr(&sec("summary").join(" ")),
        list(exps.iter().map(|e| format!(r#"{{"title":{},"company":{},"location":{},"from":{},"to":{},"bullets":{}}}"#,
            jstr(&e.title), jstr(&e.company), jstr(&e.location), jstr(&e.from), jstr(&e.raw_to), strs(&e.description))).collect()),
        list(edus.iter().map(|e| format!(r#"{{"school":{},"location":{},"degree":{},"major":{},"from":{},"to":{}}}"#,
            jstr(&e.school), jstr(&e.location), jstr(&e.degree), jstr(&e.major), jstr(&e.from), jstr(&e.to))).collect()),
        list(skills.iter().map(|g| format!(r#"{{"group":{},"items":{}}}"#, jstr(&g.0), strs(&g.1))).collect()),
        strs(&certs),
        list(languages.iter().map(|l| jstr(l)).collect()),
    );
    Ok((h, summary, json))
}

fn jstr(s: &str) -> String {
    let mut o = String::from('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jaro_matches_workday_port() {
        assert!((jaro("MARTHA", "MARHTA") - 0.9444).abs() < 1e-3);
        assert_eq!(jaro("University of Alberta", "university of alberta"), 1.0);
        assert!(jaro("UofA", "University of Alberta") < 0.75);
        assert!(best_match("Computer Science (Minor in Mathematics)", FIELDS_OF_STUDY).1 >= 0.75);
    }

    #[test]
    fn location_and_dates() {
        assert_eq!(split_location("Renisu Precious Metals Inc. Vancouver, British Columbia"), ("Renisu Precious Metals Inc.".into(), "Vancouver, British Columbia".into()));
        let c = range_re().captures("Software Engineer Feb 2026 – Present").unwrap();
        assert_eq!((&c[1], &c[2]), ("Feb 2026", "Present"));
        assert!(range_re().is_match("2018 to 2020"));
    }

    #[test]
    fn experience_and_skills() {
        let lines = ["Acme Corp Toronto, ON", "Engineer Jan 2020 – Now", "Did things.", "Senior PM | Google 2018 - 2019"];
        let e = parse_experience(&lines, &range_re());
        let apy = parse_experience(&["ApyHub Vancouver, British Columbia", "Engineer Feb 2026 – Present"], &range_re());
        assert_eq!((apy[0].company.as_str(), apy[0].location.as_str()), ("ApyHub", "Vancouver, British Columbia"));
        assert_eq!((e[0].company.as_str(), e[0].title.as_str(), e[0].to.is_none()), ("Acme Corp", "Engineer", true));
        assert_eq!((e[1].title.as_str(), e[1].company.as_str()), ("Senior PM", "Google"));
        let one_per_line = ["Acme", "Toronto, ON", "Engineer", "Jan 2020 – Present", "Shipped it.", "Beta Inc.", "Analyst", "2018 - 2019"];
        let e = parse_experience(&one_per_line, &range_re());
        assert_eq!((e[0].title.as_str(), e[0].company.as_str(), e[0].location.as_str()), ("Engineer", "Acme", "Toronto, ON"));
        assert_eq!((e[1].title.as_str(), e[1].company.as_str()), ("Analyst", "Beta Inc."));
        assert_eq!(e[0].description, vec!["Shipped it."]);
        let (ed, stray) = parse_education(&["University of Alberta", "Edmonton, Alberta", "B.Sc. Computer Science", "Sep 2018 – Jun 2022", "CompTIA Security+", "Sep 2024"], &range_re(), &single_date_re());
        assert_eq!((ed[0].location.as_str(), ed[0].from.as_str(), ed[0].to.as_str(), ed[0].major.as_str()), ("Edmonton, Alberta", "Sep 2018", "Jun 2022", "Computer Science"));
        assert_eq!(stray, vec!["CompTIA Security+"]);
        assert_eq!(split_skills("AWS (EC2, S3), Docker; Rust"), vec!["AWS (EC2, S3)", "Docker", "Rust"]);
    }

    #[test]
    fn generated_docx_reads_back() {
        let tmp = std::env::temp_dir().join(format!("ats-viewer-test-{}.docx", std::process::id()));
        let paras = r#"<w:p><w:r><w:t>Jane Doe</w:t></w:r></w:p><w:p><w:r><w:t>Acme</w:t></w:r><w:r><w:tab/></w:r><w:r><w:t>Toronto, ON</w:t></w:r></w:p>"#;
        fs::write(&tmp, web::build_docx(paras).unwrap()).unwrap();
        let text = read_docx(&tmp);
        let _ = fs::remove_file(&tmp);
        assert_eq!(text.unwrap().trim(), "Jane Doe\nAcme\tToronto, ON");
        assert_eq!(jstr("a\"b\\\n"), r#""a\"b\\\u000a""#);
    }

    #[test]
    fn docx_xml_to_text() {
        let xml = r#"<w:p><w:r><w:t>Jane</w:t></w:r><w:r><w:t xml:space="preserve"> Doe &amp; Co</w:t></w:r></w:p><w:p><w:r><w:t>A</w:t><w:tab/><w:t>B</w:t><w:br/><w:t>C</w:t></w:r></w:p><mc:Fallback><w:t>dup</w:t></mc:Fallback>"#;
        assert_eq!(docx_xml_text(xml), "Jane Doe & Co\nA\tB\nC\n");
    }
}
