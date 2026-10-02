//! CLI and process behavior of the real `switchyard` binary: data-directory locking, the
//! `token-path` and `import` subcommands against a running or stopped gateway, signal shutdown
//! and startup validation. Every process gets a private HOME, data directory and port, and only
//! fake credential files are imported.

use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
};

const BIN: &str = env!("CARGO_BIN_EXE_switchyard");

struct Sandbox {
    _root: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
    files: PathBuf,
}
fn sandbox() -> Sandbox {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let files = root.path().join("files");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&files).unwrap();
    // Not created: the gateway creates its own private data directory.
    let data = root.path().join("state");
    Sandbox {
        home,
        data,
        files,
        _root: root,
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A command isolated from the developer's environment.
fn switchyard(s: &Sandbox, port: u16, args: &[&str]) -> Command {
    let mut c = Command::new(BIN);
    c.arg("--data-dir")
        .arg(&s.data)
        .arg("--port")
        .arg(port.to_string())
        .args(args);
    for var in [
        "SWITCHYARD_HOST",
        "SWITCHYARD_PORT",
        "SWITCHYARD_DATA_DIR",
        "SWITCHYARD_PUBLIC_ORIGIN",
        "CODEX_HOME",
        "CLAUDE_CONFIG_DIR",
        "XDG_DATA_HOME",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        c.env_remove(var);
    }
    c.env("HOME", &s.home)
        .env("USERPROFILE", &s.home)
        .env("NO_PROXY", "*")
        .env("RUST_LOG", "switchyard=info");
    c.stdin(Stdio::null()).kill_on_drop(true);
    c
}

struct Output {
    ok: bool,
    stdout: String,
    stderr: String,
}
async fn run(mut c: Command) -> Output {
    let o = tokio::time::timeout(Duration::from_secs(60), c.output())
        .await
        .expect("command finished")
        .unwrap();
    Output {
        ok: o.status.success(),
        stdout: String::from_utf8_lossy(&o.stdout).into(),
        stderr: String::from_utf8_lossy(&o.stderr).into(),
    }
}

struct Server {
    child: Child,
    port: u16,
    admin: String,
    http: reqwest::Client,
}
impl Server {
    async fn start(s: &Sandbox) -> Self {
        // Ports from free_port() can be taken by a parallel test before the child binds; retry.
        let mut attempt = 0;
        let (child, port, mut lines) = loop {
            attempt += 1;
            let port = free_port();
            let mut child = switchyard(s, port, &[])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
            let ready = tokio::time::timeout(Duration::from_secs(30), async {
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.contains("Switchyard ready") {
                        return true;
                    }
                }
                false
            })
            .await;
            if matches!(ready, Ok(true)) {
                break (child, port, lines);
            }
            let mut err = String::new();
            let _ = child.stderr.take().unwrap().read_to_string(&mut err).await;
            let _ = child.wait().await;
            if !(err.contains("AddrInUse") || err.contains("in use")) || attempt >= 5 {
                panic!("server did not start: {err}");
            }
        };
        // Keep draining logs so the child never blocks on a full pipe.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        let admin = std::fs::read_to_string(s.data.join("admin-token"))
            .unwrap()
            .trim()
            .to_string();
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        Self {
            child,
            port,
            admin,
            http,
        }
    }
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }
    async fn connections(&self) -> Vec<Value> {
        let r = self
            .http
            .get(self.url("/api/connections"))
            .bearer_auth(&self.admin)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.json::<Value>().await.unwrap().as_array().unwrap().clone()
    }
    async fn stop(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

fn fake_api_key_file(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(
        &p,
        json!({"OPENAI_API_KEY":"sk-fake-process-test"}).to_string(),
    )
    .unwrap();
    p
}

#[tokio::test]
async fn token_path_prints_the_path_without_creating_or_locking_state() {
    let s = sandbox();
    let o = run(switchyard(&s, free_port(), &["token-path"])).await;
    assert!(o.ok, "{}", o.stderr);
    assert_eq!(
        o.stdout.trim(),
        s.data.join("admin-token").display().to_string()
    );
    assert!(
        !s.data.exists(),
        "token-path must not create the data directory"
    );

    // It also works while a gateway holds the lock.
    let server = Server::start(&s).await;
    let o = run(switchyard(&s, server.port, &["token-path"])).await;
    assert!(
        o.ok,
        "token-path failed while the server runs: {}",
        o.stderr
    );
    assert_eq!(
        std::fs::read_to_string(o.stdout.trim()).unwrap().trim(),
        server.admin
    );
    server.stop().await;
}

#[tokio::test]
async fn second_process_on_the_same_data_directory_is_refused() {
    let s = sandbox();
    let server = Server::start(&s).await;
    let token_before = std::fs::read(s.data.join("admin-token")).unwrap();
    let o = run(switchyard(&s, free_port(), &[])).await;
    assert!(!o.ok, "second server started on a locked data directory");
    assert!(
        format!("{}{}", o.stdout, o.stderr).contains("Another Switchyard process"),
        "{} {}",
        o.stdout,
        o.stderr
    );
    assert_eq!(
        std::fs::read(s.data.join("admin-token")).unwrap(),
        token_before,
        "refused process must not touch state"
    );
    let r = server
        .http
        .get(server.url("/healthz"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "first server unaffected");
    server.stop().await;

    // Once the owner is gone, the directory is usable again.
    let again = Server::start(&s).await;
    assert_eq!(again.admin, String::from_utf8(token_before).unwrap().trim());
    again.stop().await;
}

#[tokio::test]
async fn import_while_running_goes_through_the_admin_api() {
    let s = sandbox();
    let server = Server::start(&s).await;
    let file = fake_api_key_file(&s.files, "auth.json");
    let before = std::fs::read(&file).unwrap();
    let o = run(switchyard(
        &s,
        server.port,
        &["import", "codex", "--path", file.to_str().unwrap()],
    ))
    .await;
    assert!(o.ok, "{} {}", o.stdout, o.stderr);
    assert!(o.stdout.contains("Imported 1 connection"), "{}", o.stdout);
    let conns = server.connections().await;
    assert_eq!(
        conns.len(),
        1,
        "the running gateway sees the import immediately"
    );
    assert_eq!(conns[0]["credential_source"], "api_key");
    assert!(
        !conns
            .iter()
            .any(|c| c.to_string().contains("sk-fake-process-test"))
    );
    assert_eq!(
        std::fs::read(&file).unwrap(),
        before,
        "source file untouched"
    );

    // Reimport is idempotent through the API too.
    let o = run(switchyard(
        &s,
        server.port,
        &["import", "codex", "--path", file.to_str().unwrap()],
    ))
    .await;
    assert!(o.ok, "{}", o.stderr);
    assert_eq!(server.connections().await.len(), 1);
    server.stop().await;
}

/// `--path` is relative to the shell running the CLI, not to the gateway's working directory.
#[tokio::test]
async fn import_with_relative_path_while_running_resolves_against_the_cli_directory() {
    let s = sandbox();
    let server = Server::start(&s).await;
    fake_api_key_file(&s.files, "relative-auth.json");
    let mut cmd = switchyard(
        &s,
        server.port,
        &["import", "codex", "--path", "relative-auth.json"],
    );
    cmd.current_dir(&s.files);
    let o = run(cmd).await;
    assert!(
        o.ok,
        "relative --path failed through the running gateway: {} {}",
        o.stdout, o.stderr
    );
    assert_eq!(server.connections().await.len(), 1);
    server.stop().await;
}

/// The gateway's validation message is the useful part of a failed import.
#[tokio::test]
async fn import_errors_from_the_running_gateway_are_explained() {
    let s = sandbox();
    let server = Server::start(&s).await;
    let bad = s.files.join("broken.json");
    std::fs::write(&bad, "{not json").unwrap();
    let o = run(switchyard(
        &s,
        server.port,
        &["import", "codex", "--path", bad.to_str().unwrap()],
    ))
    .await;
    assert!(!o.ok);
    let out = format!("{}{}", o.stdout, o.stderr);
    assert!(
        out.contains("Invalid credential JSON"),
        "CLI hid the gateway's reason: {out}"
    );
    assert!(server.connections().await.is_empty());
    server.stop().await;
}

#[tokio::test]
async fn import_with_the_gateway_stopped_writes_locally() {
    let s = sandbox();
    let server = Server::start(&s).await;
    let port = server.port;
    server.stop().await;
    let file = fake_api_key_file(&s.files, "auth.json");
    let o = run(switchyard(
        &s,
        port,
        &["import", "codex", "--path", file.to_str().unwrap()],
    ))
    .await;
    assert!(o.ok, "{} {}", o.stdout, o.stderr);
    let server = Server::start(&s).await;
    assert_eq!(
        server.connections().await.len(),
        1,
        "offline import persisted"
    );
    server.stop().await;
}

/// When no gateway owns the data directory, the CLI must not hand the admin token to whatever
/// else is listening on the configured port.
#[tokio::test]
async fn import_never_sends_the_admin_token_to_a_foreign_listener() {
    let s = sandbox();
    let server = Server::start(&s).await;
    let admin = server.admin.clone();
    server.stop().await;

    let foreign = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = foreign.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = foreign.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16384];
                if let Ok(Ok(n)) =
                    tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf)).await
                {
                    log.lock().await.extend_from_slice(&buf[..n]);
                }
                let body = r#"{"imported":1,"connections":[{}]}"#;
                let _ = sock
                    .write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len()).as_bytes())
                    .await;
            });
        }
    });
    let file = fake_api_key_file(&s.files, "auth.json");
    let o = run(switchyard(
        &s,
        port,
        &["import", "codex", "--path", file.to_str().unwrap()],
    ))
    .await;
    let captured = String::from_utf8_lossy(&seen.lock().await).to_string();
    assert!(
        !captured.contains(&admin),
        "admin token was sent to a process that is not the gateway"
    );
    assert!(
        o.ok,
        "import should have completed locally: {} {}",
        o.stdout, o.stderr
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_shuts_down_gracefully_and_releases_the_lock() {
    let s = sandbox();
    let mut server = Server::start(&s).await;
    let pid = server.child.id().unwrap().to_string();
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap();
    assert!(killed.success());
    let status = tokio::time::timeout(Duration::from_secs(15), server.child.wait())
        .await
        .expect("exited after SIGTERM")
        .unwrap();
    assert!(
        status.success(),
        "graceful shutdown should exit 0: {status:?}"
    );
    let again = Server::start(&s).await;
    again.stop().await;
}

#[tokio::test]
async fn invalid_startup_configuration_is_refused() {
    let s = sandbox();
    for args in [
        ["--max-in-flight", "0"],
        ["--max-in-flight", "5000"],
        ["--timeout", "0"],
        ["--timeout", "86401"],
    ] {
        let o = run(switchyard(&s, free_port(), &args)).await;
        assert!(!o.ok, "{args:?} accepted");
    }
    assert!(!s.data.exists(), "rejected limits must not create state");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&s.data).unwrap();
        std::fs::set_permissions(&s.data, std::fs::Permissions::from_mode(0o755)).unwrap();
        let o = run(switchyard(&s, free_port(), &[])).await;
        assert!(!o.ok, "shared data directory accepted");
        assert!(
            format!("{}{}", o.stdout, o.stderr).contains("private"),
            "{}",
            o.stderr
        );
        let mode = std::fs::metadata(&s.data).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "permissions must not be changed silently");
        assert!(
            std::fs::read_dir(&s.data).unwrap().next().is_none(),
            "nothing written to a refused directory"
        );
    }
}
