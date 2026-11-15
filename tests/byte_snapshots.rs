//! End to end tests driving the built binary against a local HTTP server.
//!
//! The server answers with multiple content types and encodings (binary PDF /
//! PNG, UTF-8 HTML, UTF-16 pages, JSON and a redirect) so the byte snapshot
//! behaviour can be verified on real network responses.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::thread;

/// A response the test server sends back.
struct MockResponse {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

fn binary_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/website-stalker")
}

/// Start a single threaded HTTP/1.1 server and return its base URL.
///
/// `routes` maps the request path to a response builder. The builder receives
/// the run counter so responses can change between requests.
fn start_server<F>(routes: F) -> String
where
    F: Fn(&str, u64) -> MockResponse + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let port = listener.local_addr().expect("local addr").port();
    thread::spawn(move || {
        let mut run = 0;
        for stream in listener.incoming() {
            let Ok(mut connection) = stream else {
                continue;
            };
            run += 1;
            handle_connection(&mut connection, &routes, run);
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn handle_connection<F>(stream: &mut TcpStream, routes: &F, run: u64)
where
    F: Fn(&str, u64) -> MockResponse,
{
    let mut buffer = [0; 1024];
    let read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1));
    let path = path.unwrap_or("/");

    let MockResponse {
        status,
        content_type,
        body,
    } = routes(path, run);

    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
    stream.write_all(&body).unwrap();
    stream.flush().unwrap();
}

fn sha256(path: &Path) -> Vec<u8> {
    use std::collections::hash_map::DefaultHasher;
    use std::fs::read;
    use std::hash::{Hash as _, Hasher as _};
    // A plain hash is enough to compare byte identity within this test.
    let mut hasher = DefaultHasher::new();
    read(path).unwrap().hash(&mut hasher);
    hasher.finish().to_le_bytes().to_vec()
}

fn run_binary(workdir: &Path, config: &str) -> String {
    std::fs::write(workdir.join("website-stalker.yaml"), config).unwrap();
    let output = Command::new(binary_path())
        .current_dir(workdir)
        .args(["run", "--all", "--from", "test@example.com"])
        .output()
        .expect("run website-stalker");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "binary failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    stderr
}

/// Output directory for a server on 127.0.0.1 with the given port.
/// The domain folder is `127-0-0-1/<port>/`.
fn site_folder(workdir: &Path, base_url: &str) -> std::path::PathBuf {
    let port = base_url.rsplit(':').next().unwrap();
    workdir.join("127-0-0-1").join(port)
}

fn config_for(base_url: &str, sites: &str) -> String {
    format!(
        "from: test@example.com\nsites:\n{sites}",
        sites = sites.replace("{BASE}", base_url)
    )
}

#[test]
fn byte_snapshots_for_binary_and_multi_encoding_text() {
    // A minimal but valid PDF header/trailer and a PNG magic byte sequence.
    let pdf = b"%PDF-1.4\n1 0 obj<<>>endobj\ntrailer<<>>\n%%EOF\n".to_vec();
    let png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0xFF];

    // UTF-16LE and UTF-16BE encoded "Héllo" with BOMs.
    let little_endian_page: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain("Héllo".encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    let big_endian_page: Vec<u8> = [0xFE, 0xFF]
        .into_iter()
        .chain("Héllo".encode_utf16().flat_map(u16::to_be_bytes))
        .collect();
    let html = b"<html><body><h1>Hello</h1></body></html>".to_vec();
    let json = br#"{"name":"stalker","value":42}"#.to_vec();

    let pdf_clone = pdf.clone();
    let png_clone = png.clone();
    let le_clone = little_endian_page.clone();
    let be_clone = big_endian_page.clone();
    let html_clone = html;
    let json_clone = json;
    let base_url = start_server(move |path, _run| match path {
        "/document.pdf" => MockResponse {
            status: "200 OK",
            content_type: "application/pdf",
            body: pdf_clone.clone(),
        },
        "/picture.png" => MockResponse {
            status: "200 OK",
            content_type: "image/png",
            body: png_clone.clone(),
        },
        "/utf16le" => MockResponse {
            status: "200 OK",
            content_type: "text/html; charset=UTF-16LE",
            body: le_clone.clone(),
        },
        "/utf16be" => MockResponse {
            status: "200 OK",
            content_type: "text/html; charset=UTF-16BE",
            body: be_clone.clone(),
        },
        "/html" => MockResponse {
            status: "200 OK",
            content_type: "text/html; charset=UTF-8",
            body: html_clone.clone(),
        },
        "/data.json" => MockResponse {
            status: "200 OK",
            content_type: "application/json",
            body: json_clone.clone(),
        },
        _ => MockResponse {
            status: "404 NOT FOUND",
            content_type: "text/plain",
            body: b"not found".to_vec(),
        },
    });

    let workdir = tempfile::tempdir().unwrap();
    let workdir_path = workdir.path();
    let config = config_for(
        &base_url,
        r#"  - url: "{BASE}/document.pdf"
  - url: "{BASE}/picture.png"
  - url: "{BASE}/utf16le"
  - url: "{BASE}/utf16be"
  - url: "{BASE}/html"
    editors:
      - html_textify
  - url: "{BASE}/data.json"
    editors:
      - json_prettify
"#,
    );
    run_binary(workdir_path, &config);

    let folder = site_folder(workdir_path, &base_url);
    let pdf_file = folder.join("document-pdf.pdf");
    let png_file = folder.join("picture-png.png");
    let le_file = folder.join("utf16le.html");
    let be_file = folder.join("utf16be.html");
    let text_file = folder.join("html.txt");
    let json_file = folder.join("data-json.json");

    // Binary content is stored byte for byte.
    assert_eq!(std::fs::read(&pdf_file).unwrap(), pdf);
    assert_eq!(std::fs::read(&png_file).unwrap(), png);

    // UTF-16 pages without editors keep their exact original bytes.
    assert_eq!(std::fs::read(&le_file).unwrap(), little_endian_page);
    assert_eq!(std::fs::read(&be_file).unwrap(), big_endian_page);

    // The html_textify editor decoded UTF-8 and produced plain text.
    assert_eq!(std::fs::read_to_string(&text_file).unwrap().trim(), "Hello");

    // json_prettify decoded and pretty printed the JSON.
    let pretty = std::fs::read_to_string(&json_file).unwrap();
    assert!(pretty.contains('\n'));
    assert!(pretty.contains("\"name\": \"stalker\""));

    // No temporary files are left behind.
    for entry in walkdir(&folder) {
        assert!(
            !entry.to_string_lossy().ends_with(".tmp"),
            "temp file left behind: {entry:?}"
        );
    }
}

#[test]
fn consecutive_runs_keep_identical_snapshots() {
    let pdf = b"%PDF-1.4 unchanged bytes %%EOF".to_vec();
    let html = b"<html><body>Same</body></html>".to_vec();
    let pdf_clone = pdf;
    let html_clone = html;
    let base_url = start_server(move |path, _run| match path {
        "/document.pdf" => MockResponse {
            status: "200 OK",
            content_type: "application/pdf",
            body: pdf_clone.clone(),
        },
        _ => MockResponse {
            status: "200 OK",
            content_type: "text/html; charset=UTF-8",
            body: html_clone.clone(),
        },
    });

    let workdir = tempfile::tempdir().unwrap();
    let config = config_for(
        &base_url,
        r#"  - url: "{BASE}/document.pdf"
  - url: "{BASE}/html"
"#,
    );

    run_binary(workdir.path(), &config);
    let folder = site_folder(workdir.path(), &base_url);
    let pdf_file = folder.join("document-pdf.pdf");
    let html_file = folder.join("html.html");
    let first_pdf_hash = sha256(&pdf_file);
    let first_html_hash = sha256(&html_file);
    let first_pdf_mtime = std::fs::metadata(&pdf_file).unwrap().modified().unwrap();

    // Wait enough that a rewrite would produce a newer modification time.
    thread::sleep(std::time::Duration::from_millis(20));
    let stderr = run_binary(workdir.path(), &config);

    // Unchanged content means ContentSame and no rewrite.
    assert!(stderr.contains("ContentSame"));
    assert_eq!(sha256(&pdf_file), first_pdf_hash);
    assert_eq!(sha256(&html_file), first_html_hash);
    assert_eq!(
        std::fs::metadata(&pdf_file).unwrap().modified().unwrap(),
        first_pdf_mtime,
        "unchanged snapshot was rewritten"
    );
}

#[test]
fn follows_redirect_and_uses_final_url_extension() {
    let target = b"<html><body>redirected body</body></html>".to_vec();

    // Dedicated server which actually sends a Location header for /redirect.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let target_body = target.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut buffer = [0; 1024];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");
            if path == "/redirect" {
                let response = format!(
                    "HTTP/1.1 302 FOUND\r\nLocation: http://127.0.0.1:{port}/target.html\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                );
                stream.write_all(response.as_bytes()).unwrap();
            } else {
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=UTF-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    target_body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(&target_body).unwrap();
            }
            stream.flush().unwrap();
        }
    });
    let base_url = format!("http://127.0.0.1:{port}");

    let workdir = tempfile::tempdir().unwrap();
    let config = config_for(
        &base_url,
        r#"  - url: "{BASE}/redirect"
"#,
    );
    let stderr = run_binary(workdir.path(), &config);

    // The redirect warning is emitted and the final content is stored.
    assert!(stderr.contains("redirected"));
    let folder = site_folder(workdir.path(), &base_url);
    // Basename comes from the configured URL (redirect), extension from the
    // final URL / content type (html).
    let stored = folder.join("redirect.html");
    assert_eq!(std::fs::read(&stored).unwrap(), target);
}

#[test]
fn type_change_removes_stale_extension_file() {
    // First run answers HTML, second run answers plain text.
    let base_url = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let mut runs: HashMap<String, u64> = HashMap::new();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut buffer = [0; 1024];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_owned();
                let count = runs.entry(path.clone()).or_insert(0);
                *count += 1;
                let run = *count;
                let (content_type, body): (&str, Vec<u8>) = if run == 1 {
                    ("text/html; charset=UTF-8", b"<html>first</html>".to_vec())
                } else {
                    ("text/plain; charset=UTF-8", b"second".to_vec())
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
                stream.flush().unwrap();
            }
        });
        format!("http://127.0.0.1:{port}")
    };

    let workdir = tempfile::tempdir().unwrap();
    let config = config_for(
        &base_url,
        r#"  - url: "{BASE}/typechange"
"#,
    );
    run_binary(workdir.path(), &config);
    let folder = site_folder(workdir.path(), &base_url);
    let html_file = folder.join("typechange.html");
    assert!(html_file.exists());

    run_binary(workdir.path(), &config);

    // The type changed so the old .html file is gone and .txt exists instead.
    let text_file = folder.join("typechange.txt");
    assert!(text_file.exists(), "new .txt snapshot should exist");
    assert!(
        !html_file.exists(),
        "stale .html snapshot should be removed"
    );
    assert_eq!(
        std::fs::read_to_string(&text_file).unwrap().trim(),
        "second"
    );
}

#[test]
fn editors_on_binary_response_fail_without_corrupting_snapshot() {
    let pdf = b"%PDF-1.4 binary %%EOF".to_vec();
    let pdf_clone = pdf;
    let base_url = start_server(move |path, _run| match path {
        "/document.pdf" => MockResponse {
            status: "200 OK",
            content_type: "application/pdf",
            body: pdf_clone.clone(),
        },
        _ => MockResponse {
            status: "404 NOT FOUND",
            content_type: "text/plain",
            body: Vec::new(),
        },
    });

    let workdir = tempfile::tempdir().unwrap();
    let config = config_for(
        &base_url,
        r#"  - url: "{BASE}/document.pdf"
    editors:
      - html_textify
"#,
    );
    std::fs::write(workdir.path().join("website-stalker.yaml"), config).unwrap();
    let output = Command::new(binary_path())
        .current_dir(workdir.path())
        .args(["run", "--all", "--from", "test@example.com"])
        .output()
        .expect("run website-stalker");
    assert!(
        !output.status.success(),
        "binary should reject binary editors"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not text content"),
        "expected a clear text-type error, got: {stderr}"
    );
    // Nothing was written for the failed site.
    let folder = site_folder(workdir.path(), &base_url);
    assert!(
        !folder.exists() || std::fs::read_dir(&folder).unwrap().next().is_none(),
        "no snapshot should be created on failure"
    );
}

#[test]
fn utf16_charset_is_sniffed_from_bom_without_header_charset() {
    // text/html without any charset parameter. The BOM must identify the
    // endianness so the html_textify editor can decode the page.
    let little_endian: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain(
            "<html><body>Little Endian</body></html>"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        )
        .collect();
    let big_endian: Vec<u8> = [0xFE, 0xFF]
        .into_iter()
        .chain(
            "<html><body>Big Endian</body></html>"
                .encode_utf16()
                .flat_map(u16::to_be_bytes),
        )
        .collect();
    let little_clone = little_endian;
    let big_clone = big_endian;
    let base_url = start_server(move |path, _run| match path {
        "/le" => MockResponse {
            status: "200 OK",
            content_type: "text/html",
            body: little_clone.clone(),
        },
        _ => MockResponse {
            status: "200 OK",
            content_type: "text/html",
            body: big_clone.clone(),
        },
    });

    let workdir = tempfile::tempdir().unwrap();
    let config = config_for(
        &base_url,
        r#"  - url: "{BASE}/le"
    editors:
      - html_textify
  - url: "{BASE}/be"
    editors:
      - html_textify
"#,
    );
    run_binary(workdir.path(), &config);

    let folder = site_folder(workdir.path(), &base_url);
    let le = std::fs::read_to_string(folder.join("le.txt")).unwrap();
    let be = std::fs::read_to_string(folder.join("be.txt")).unwrap();
    assert_eq!(le.trim(), "Little Endian");
    assert_eq!(be.trim(), "Big Endian");
}

fn walkdir(path: &Path) -> Vec<std::path::PathBuf> {
    let mut result = Vec::new();
    if path.is_file() {
        result.push(path.to_path_buf());
    } else if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            result.extend(walkdir(&entry.path()));
        }
    }
    result
}
