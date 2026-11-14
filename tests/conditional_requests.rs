//! End to end tests for persistent conditional requests (`ETag` / `Last-Modified`).
//!
//! A local HTTP server scripts the responses for four consecutive runs:
//! `200` -> `304` -> `500` -> `200` (changed content).
//! Each run executes the real built binary in a temporary git repository so
//! request headers, exit codes, snapshot files and git history can be checked.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

const BIN: &str = env!("CARGO_BIN_EXE_website-stalker");
const ETAG_V1: &str = "v1";
const ETAG_V2: &str = "v2";
const ETAG_V1_HEADER: &str = "\"v1\"";
const LAST_MODIFIED: &str = "Wed, 21 Oct 2015 07:28:00 GMT";

#[derive(Clone, Debug, PartialEq, Eq)]
struct RecordedRequest {
    target: String,
    if_none_match: Option<String>,
    if_modified_since: Option<String>,
    from: Option<String>,
}

struct ScriptedResponse {
    status: &'static str,
    headers: &'static str,
    body: &'static str,
}

fn start_server() -> (u16, Arc<Mutex<Vec<RecordedRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server_requests = Arc::clone(&requests);

    let script = [
        // Run 1: initial fetch, no validators known yet.
        ScriptedResponse {
            status: "200 OK",
            headers: concat!(
                "ETag: \"v1\"\r\n",
                "Last-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n",
                "Content-Type: text/plain"
            ),
            body: "first version",
        },
        // Run 2: conditional request confirms nothing changed.
        ScriptedResponse {
            status: "304 Not Modified",
            headers: concat!(
                "ETag: \"v1\"\r\n",
                "Last-Modified: Wed, 21 Oct 2015 07:28:00 GMT"
            ),
            body: "",
        },
        // Run 3: temporary server failure.
        ScriptedResponse {
            status: "500 Internal Server Error",
            headers: "Content-Type: text/plain",
            body: "boom",
        },
        // Run 4: content changed, new validators.
        ScriptedResponse {
            status: "200 OK",
            headers: concat!(
                "ETag: \"v2\"\r\n",
                "Last-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n",
                "Content-Type: text/plain"
            ),
            body: "second version",
        },
    ];

    std::thread::spawn(move || {
        for response in &script {
            let mut stream = listener.accept().unwrap().0;
            let request = read_request(&mut stream);
            server_requests.lock().unwrap().push(request);
            write_response(&mut stream, response);
        }
    });

    (port, requests)
}

fn read_request(stream: &mut TcpStream) -> RecordedRequest {
    let mut buffer = Vec::new();
    let mut chunk = [0; 1024];
    while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
        let read = stream.read(&mut chunk).unwrap();
        assert!(read > 0, "connection closed before request headers arrived");
        buffer.extend_from_slice(&chunk[..read]);
    }
    let raw = String::from_utf8(buffer).unwrap();
    let mut lines = raw.lines();
    let request_line = lines.next().unwrap();
    let target = request_line.split_whitespace().nth(1).unwrap().to_owned();
    let mut headers = std::collections::HashMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    RecordedRequest {
        target,
        if_none_match: headers.get("if-none-match").cloned(),
        if_modified_since: headers.get("if-modified-since").cloned(),
        from: headers.get("from").cloned(),
    }
}

fn write_response(stream: &mut TcpStream, response: &ScriptedResponse) {
    let body = response.body.as_bytes();
    let reply = format!(
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n\r\n",
        response.status,
        body.len(),
        response.headers,
    );
    stream.write_all(reply.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn run_binary(dir: &Path, args: &[&str]) -> std::process::ExitStatus {
    Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap()
}

fn snapshot_path(dir: &Path, port: u16) -> std::path::PathBuf {
    dir.join(format!("127-0-0-1/{port}/page.txt"))
}

fn write_config(dir: &Path, port: u16) {
    let config = format!("from: test@example.com\nsites:\n  - url: http://127.0.0.1:{port}/page\n");
    std::fs::write(dir.join("website-stalker.yaml"), config).unwrap();
}

#[test]
fn conditional_requests_304_and_failures() {
    let tempdir = tempfile::tempdir().unwrap();
    let dir = tempdir.path();
    git(dir, &["init"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);

    let (port, requests) = start_server();
    write_config(dir, port);
    // The config has to be committed already: --commit requires a clean repo.
    git(dir, &["add", "website-stalker.yaml"]);
    git(dir, &["commit", "-m", "config"]);
    let snapshot = snapshot_path(dir, port);
    let state_file = dir.join(".git/website-stalker/state.json");

    // Run 1: plain GET (no stored validators), 200 with ETag/Last-Modified.
    // --commit on a clean repo must work even though the state is created.
    assert!(run_binary(dir, &["run", "--all", "--commit"]).success());
    assert_eq!(
        std::fs::read_to_string(&snapshot).unwrap(),
        "first version\n"
    );
    assert!(state_file.exists(), "persistent state was created");
    let state = std::fs::read_to_string(&state_file).unwrap();
    assert!(state.contains(ETAG_V1), "state stores the ETag: {state}");
    assert!(state.contains(LAST_MODIFIED), "state stores Last-Modified");
    assert!(
        state.contains(&format!("127.0.0.1:{port}/page")),
        "state stores the final URL: {state}"
    );

    // Run 2: 304 keeps the snapshot and reports no content change.
    let modified_before = std::fs::metadata(&snapshot).unwrap().modified().unwrap();
    assert!(run_binary(dir, &["run", "--all"]).success());
    assert_eq!(
        std::fs::read_to_string(&snapshot).unwrap(),
        "first version\n"
    );
    assert_eq!(
        std::fs::metadata(&snapshot).unwrap().modified().unwrap(),
        modified_before,
        "304 must not rewrite the snapshot"
    );
    assert_empty(&git(dir, &["status", "--short"]));

    // Run 3: temporary failure keeps snapshot and state, exits non-zero.
    let status = run_binary(dir, &["run", "--all"]);
    assert!(!status.success(), "a 500 response results in exit code 1");
    assert_eq!(
        std::fs::read_to_string(&snapshot).unwrap(),
        "first version\n"
    );
    assert!(
        std::fs::read_to_string(&state_file)
            .unwrap()
            .contains(ETAG_V1),
        "a failed run must not alter the stored state"
    );

    // Run 4: real content change with --commit creates exactly one new commit.
    assert!(run_binary(dir, &["run", "--all", "--commit"]).success());
    assert_eq!(
        std::fs::read_to_string(&snapshot).unwrap(),
        "second version\n"
    );
    assert!(
        std::fs::read_to_string(&state_file)
            .unwrap()
            .contains(ETAG_V2)
    );

    // Check the recorded requests and their conditional headers.
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].from.as_deref(), Some("test@example.com"));
    assert!(
        requests[0].if_none_match.is_none(),
        "the first request must not be conditional"
    );
    for index in [1, 2, 3] {
        assert_eq!(
            requests[index].if_none_match.as_deref(),
            Some(ETAG_V1_HEADER),
            "request {index} should reuse the stored ETag"
        );
        assert_eq!(
            requests[index].if_modified_since.as_deref(),
            Some(LAST_MODIFIED),
            "request {index} should reuse the stored Last-Modified"
        );
    }

    // Git history: two website-stalker commits (on top of the config commit)
    // and the state file is never tracked.
    let log = git(dir, &["log", "--oneline"]);
    assert_eq!(log.lines().count(), 3, "unexpected git history: {log}");
    let tracked = git(dir, &["ls-files"]);
    assert!(tracked.contains("website-stalker.yaml"));
    assert!(tracked.contains(&format!("127-0-0-1/{port}/page.txt")));
    assert!(
        !tracked.contains("website-stalker/state.json"),
        "the state must stay out of git: {tracked}"
    );
    let head_stat = git(dir, &["show", "--stat", "--oneline", "HEAD"]);
    assert!(
        head_stat.contains(&format!("127-0-0-1/{port}/page.txt")),
        "the change commit contains the snapshot: {head_stat}"
    );
    assert!(
        !head_stat.contains("state.json"),
        "the change commit must not contain the state: {head_stat}"
    );
    assert_empty(&git(dir, &["status", "--short"]));
}

fn assert_empty(text: &str) {
    assert_eq!(text, "");
}

#[test]
fn works_without_git_repository() {
    // Old/alternative setups without git keep working and still persist state
    // in a hidden working directory.
    let tempdir = tempfile::tempdir().unwrap();
    let dir = tempdir.path();
    // This server's first scripted response is the initial 200.
    let (port, _requests) = start_server();
    write_config(dir, port);
    let snapshot = snapshot_path(dir, port);

    assert!(run_binary(dir, &["run", "--all"]).success());
    assert_eq!(
        std::fs::read_to_string(&snapshot).unwrap(),
        "first version\n"
    );
    let state_file = dir.join(".website-stalker/state.json");
    assert!(state_file.exists());
    assert!(
        std::fs::read_to_string(&state_file)
            .unwrap()
            .contains(ETAG_V1)
    );
}

/// Server which redirects `/old` to `/new` once and answers `/new` with
/// `200` followed by `304`. Returns all recorded request targets.
fn start_redirect_server() -> (u16, Arc<Mutex<Vec<RecordedRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server_requests = Arc::clone(&requests);

    std::thread::spawn(move || {
        loop {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let request = read_request(&mut stream);
            let target = request.target.clone();
            server_requests.lock().unwrap().push(request);

            if target == "/old" {
                let reply = "HTTP/1.1 301 Moved Permanently\r\nLocation: /new\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                stream.write_all(reply.as_bytes()).unwrap();
                stream.flush().unwrap();
                continue;
            }

            let not_modified_yet = server_requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.target == "/new")
                .count()
                > 1;
            if not_modified_yet {
                write_response(
                    &mut stream,
                    &ScriptedResponse {
                        status: "304 Not Modified",
                        headers: concat!(
                            "ETag: \"v1\"\r\n",
                            "Last-Modified: Wed, 21 Oct 2015 07:28:00 GMT"
                        ),
                        body: "",
                    },
                );
            } else {
                write_response(
                    &mut stream,
                    &ScriptedResponse {
                        status: "200 OK",
                        headers: concat!(
                            "ETag: \"v1\"\r\n",
                            "Last-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n",
                            "Content-Type: text/plain"
                        ),
                        body: "redirected content",
                    },
                );
            }
        }
    });

    (port, requests)
}

#[test]
fn final_redirect_url_is_reused() {
    let tempdir = tempfile::tempdir().unwrap();
    let dir = tempdir.path();
    let (port, requests) = start_redirect_server();

    let config = format!("from: test@example.com\nsites:\n  - url: http://127.0.0.1:{port}/old\n");
    std::fs::write(dir.join("website-stalker.yaml"), config).unwrap();
    let snapshot = dir.join(format!("127-0-0-1/{port}/old.txt"));

    // Run 1: follows the redirect and stores the final URL.
    assert!(run_binary(dir, &["run", "--all"]).success());
    assert_eq!(
        std::fs::read_to_string(&snapshot).unwrap(),
        "redirected content\n"
    );
    let state = std::fs::read_to_string(dir.join(".website-stalker/state.json")).unwrap();
    assert!(
        state.contains(&format!("127.0.0.1:{port}/new")),
        "the final URL after redirects is stored: {state}"
    );

    // Run 2: requests the stored final URL directly, no redirect involved.
    assert!(run_binary(dir, &["run", "--all"]).success());

    let requests = requests.lock().unwrap();
    let targets = requests
        .iter()
        .map(|request| request.target.as_str())
        .collect::<Vec<_>>();
    // Run 1: /old (redirect) then /new. Run 2: /new directly, no /old.
    assert_eq!(targets, ["/old", "/new", "/new"], "unexpected: {targets:?}");
    assert_eq!(
        requests[2].if_none_match.as_deref(),
        Some(ETAG_V1_HEADER),
        "the direct request to the final URL stays conditional"
    );
}
