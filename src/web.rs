//! `ats-viewer serve`: local upload page and resume editor. Binds to 127.0.0.1 only; uploads are
//! written to a private temp file for parsing and deleted immediately after.

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Query},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

const MAX_UPLOAD: usize = 20 * 1024 * 1024;
const TYPES: &[&str] = &["pdf", "docx", "html", "htm", "txt"];

pub fn serve(port: u16) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let app = Router::new()
            .route("/", get(|| async { Html(PAGE) }))
            .route("/analyze", post(analyze_upload))
            .route("/editor", get(|| async { Html(EDITOR) }))
            .route("/docx", post(docx))
            .layer(DefaultBodyLimit::max(MAX_UPLOAD));
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap_or_else(|e| {
            eprintln!("cannot bind {addr}: {e}");
            std::process::exit(1);
        });
        println!("ats-viewer: open http://{addr}  (local only; uploads are deleted after parsing)");
        axum::serve(listener, app).await.expect("server error");
    });
}

/// Body is the raw file; `?name=` carries the original filename (for the extension and report title).
/// `?format=json` returns the parsed fields instead of the report (editor import).
async fn analyze_upload(Query(q): Query<HashMap<String, String>>, body: Bytes) -> Response {
    let name = q.get("name").cloned().unwrap_or_default();
    let json = q.get("format").is_some_and(|f| f == "json");
    let ext = Path::new(&name).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if !TYPES.contains(&ext.as_str()) {
        return (StatusCode::BAD_REQUEST, format!("Unsupported file type '.{ext}'. Use PDF, DOCX, HTML or TXT.")).into_response();
    }
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "The file is empty.").into_response();
    }
    static N: AtomicU64 = AtomicU64::new(0);
    let tmp = std::env::temp_dir().join(format!("ats-viewer-{}-{}.{ext}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let result = tokio::task::spawn_blocking(move || {
        let r = write_private(&tmp, &body).map_err(|e| format!("cannot store upload: {e}")).and_then(|_| crate::analyze(&tmp, &name));
        let _ = fs::remove_file(&tmp);
        r
    })
    .await;
    match result {
        Ok(Ok((_, _, j))) if json => ([(header::CONTENT_TYPE, "application/json")], j).into_response(),
        Ok(Ok((html, _, _))) => Html(html.replacen("<main>", "<main><p><a href=\"/\">← Analyze another resume</a></p>", 1)).into_response(),
        Ok(Err(e)) => (StatusCode::UNPROCESSABLE_ENTITY, e).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "The parser crashed on this file.").into_response(),
    }
}

/// Body is the WordprocessingML paragraphs the editor generated; returns them wrapped in a
/// minimal DOCX (Letter page, 0.6 in margins, no headers, footers, tables or text boxes).
async fn docx(body: Bytes) -> Response {
    let Ok(paras) = std::str::from_utf8(&body) else {
        return (StatusCode::BAD_REQUEST, "Body must be UTF-8 XML.").into_response();
    };
    match build_docx(paras) {
        Ok(b) => ([(header::CONTENT_TYPE, "application/vnd.openxmlformats-officedocument.wordprocessingml.document")], b).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub(crate) fn build_docx(paras: &str) -> zip::result::ZipResult<Vec<u8>> {
    use zip::write::{SimpleFileOptions, ZipWriter};
    const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    let doc = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="{W}"><w:body>{paras}<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="864" w:right="864" w:bottom="864" w:left="864" w:header="0" w:footer="0" w:gutter="0"/></w:sectPr></w:body></w:document>"#);
    let parts = [
        ("[Content_Types].xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/></Types>"#.to_string()),
        ("_rels/.rels", format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{R}/officeDocument" Target="word/document.xml"/></Relationships>"#)),
        ("word/_rels/document.xml.rels", format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{R}/styles" Target="styles.xml"/></Relationships>"#)),
        ("word/styles.xml", format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles xmlns:w="{W}"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri" w:cs="Calibri"/><w:sz w:val="21"/><w:lang w:val="en-US"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="0" w:line="252" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults></w:styles>"#)),
        ("word/document.xml", doc),
    ];
    let mut z = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, xml) in parts {
        z.start_file(name, SimpleFileOptions::default())?;
        z.write_all(xml.as_bytes())?;
    }
    Ok(z.finish()?.into_inner())
}

/// Resumes hold personal data: owner-only permissions, and create_new so an existing path is never followed.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)?.write_all(bytes)
}

const EDITOR: &str = include_str!("editor.html");

const PAGE: &str = r##"<!doctype html><html><head><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1"><title>ATS Resume Viewer</title><style>
:root{--bg:#f6f7f9;--card:#fff;--fg:#1d2330;--mut:#667085;--line:#c9ced6;--hi:#c0362c;--acc:#2456c7}
@media (prefers-color-scheme:dark){:root{--bg:#12151b;--card:#1b1f27;--fg:#e6e8ec;--mut:#98a2b3;--line:#3a414d;--hi:#f0706a;--acc:#7aa2ff}}
body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif}main{max-width:640px;margin:auto;padding:40px 16px}
h1{font-size:24px;margin:0 0 6px}p{color:var(--mut)}
#drop{display:block;border:2px dashed var(--line);border-radius:12px;background:var(--card);padding:48px 16px;text-align:center;cursor:pointer;margin:24px 0}
#drop.over,#drop:focus-within{border-color:var(--acc)}#drop strong{color:var(--acc)}input{position:absolute;opacity:0;width:1px;height:1px}
#msg{min-height:1.5em}#msg.err{color:var(--hi)}</style></head><body><main>
<h1>ATS Resume Viewer</h1>
<p>See what a recruiter gets after Workday, Lever or Greenhouse parse your resume: the profile fields, what got lost, and the raw text the parser reads.</p>
<label id=drop for=file><strong>Choose a resume</strong> or drop it here<br><span style="color:var(--mut);font-size:13px">PDF, DOCX, HTML or TXT · up to 20 MB</span>
<input id=file type=file accept=".pdf,.docx,.html,.htm,.txt"></label>
<p id=msg role=status></p>
<p><a href="/editor" style="color:var(--acc)">Or build and edit a resume in the editor →</a></p>
<p style="font-size:13px">Runs on this machine only. The file is parsed and deleted; nothing is sent anywhere else. Field values are a heuristic estimate of closed vendor parsers.</p>
</main><script>
const go=async f=>{if(!f)return;msg.className="";msg.textContent="Parsing "+f.name+"…";
try{const r=await fetch("/analyze?name="+encodeURIComponent(f.name),{method:"POST",body:f});const t=await r.text();
if(!r.ok){msg.className="err";msg.textContent=r.status==413?"File is over 20 MB.":t;return}
document.open();document.write(t);document.close()}catch(e){msg.className="err";msg.textContent="Could not reach the server: "+e.message}};
file.onchange=()=>go(file.files[0]);
["dragenter","dragover"].forEach(e=>drop.addEventListener(e,ev=>{ev.preventDefault();drop.classList.add("over")}));
["dragleave","drop"].forEach(e=>drop.addEventListener(e,ev=>{ev.preventDefault();drop.classList.remove("over")}));
drop.addEventListener("drop",ev=>go(ev.dataTransfer.files[0]));
</script></body></html>"##;
