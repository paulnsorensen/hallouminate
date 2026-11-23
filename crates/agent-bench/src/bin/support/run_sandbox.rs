use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agent_bench::Arm;
use anyhow::{Context, bail, ensure};
use serde_json::{Value, json};

const CORPUS: &str = "benchmark-wiki";
const WORKSPACE: &str = "/workspace";
const SOCKET: &str = "/run/bench/daemon.sock";
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(600);
const MODEL_CACHE: &str = "/model-cache";
const DAEMON_CONFIG: &str = r#"[daemon]
idle_exit_secs = 0

[embeddings]
cache_dir = "/model-cache/fastembed"

[[corpus]]
name = "benchmark-wiki"
paths = ["/knowledge/wiki"]
"#;
const BASELINE_MCP: &[u8] =
    include_bytes!("../../../../../eval/agent-bench/config/baseline-arm.mcp.json");
const WIKI_MCP: &[u8] = include_bytes!("../../../../../eval/agent-bench/config/wiki-arm.mcp.json");

pub(super) struct Sandbox {
    bwrap: PathBuf,
    hallouminate: PathBuf,
    model_cache: PathBuf,
}

pub(super) struct Views {
    root: PathBuf,
    page: PathBuf,
    wiki_hash: String,
    source_hash: String,
}

impl Sandbox {
    pub(super) fn new(hallouminate: PathBuf, scratch: &Path) -> anyhow::Result<Self> {
        let model_cache = scratch.join("models");
        fs::create_dir(&model_cache)?;
        Ok(Self {
            bwrap: PathBuf::from("/usr/bin/bwrap"),
            hallouminate,
            model_cache,
        })
    }

    pub(super) fn fingerprint(&self) -> anyhow::Result<String> {
        agent_bench::provenance::fingerprint(&(
            include_bytes!("run_sandbox.rs").as_slice(),
            BASELINE_MCP,
            WIKI_MCP,
            DAEMON_CONFIG,
            agent_bench::blake3_file_hash(&self.bwrap)
                .context("bench-run requires Linux Bubblewrap at /usr/bin/bwrap")?,
            agent_bench::blake3_file_hash(&self.hallouminate)?,
        ))
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.bwrap);
        command.env_clear().args([
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--setenv",
            "PATH",
            "/usr/bin:/bin",
            "--setenv",
            "HOME",
            "/home/bench",
            "--setenv",
            "TMPDIR",
            "/tmp",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/home/bench",
            "--dir",
            "/run/bench",
        ]);
        for path in [
            "/usr",
            "/bin",
            "/lib",
            "/lib64",
            "/etc/ssl/certs",
            "/etc/resolv.conf",
            "/etc/hosts",
        ] {
            if Path::new(path).exists() {
                bind_read_only(&mut command, path, path);
            }
        }
        command
    }

    pub(super) fn agent(&self, views: &Views, arm: Arm, binary: &Path) -> Command {
        let mut command = self.arm(views, arm);
        // The agent must reach the configured model service. No host home or
        // host socket directories are mounted into either measurement arm.
        command.arg("--share-net");
        for key in ["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"] {
            if let Some(value) = std::env::var_os(key) {
                // Put secrets in the environment, not in Bubblewrap's argv.
                command.env(key, value);
            }
        }
        bind_read_only(&mut command, binary, "/opt/agent");
        command.args(["--", "/opt/agent"]);
        command
    }

    fn arm(&self, views: &Views, arm: Arm) -> Command {
        let mut command = self.command();
        let root = match arm {
            Arm::Baseline => views.root.join("baseline"),
            Arm::Wiki => views.root.join("wiki"),
        };
        bind_read_only(&mut command, root.join("source"), WORKSPACE);
        bind_read_only(&mut command, root.join("mcp.json"), "/run/bench/mcp.json");
        command.args(["--chdir", WORKSPACE]);
        if arm == Arm::Wiki {
            bind_read_only(&mut command, &self.hallouminate, "/opt/hallouminate");
            bind_read_only(&mut command, views.socket(), SOCKET);
            command.args(["--setenv", "HALLOUMINATE_SOCKET", SOCKET]);
        }
        command
    }

    pub(super) fn start_daemon(&self, views: &Views) -> anyhow::Result<Daemon> {
        let mut command = self.command();
        // The daemon may fetch its default retrieval model. Its filesystem has
        // no host credentials, source checkout, or other corpora.
        command.arg("--share-net");
        bind_read_only(&mut command, &self.hallouminate, "/opt/hallouminate");
        bind_read_only(
            &mut command,
            views.root.join("wiki/data"),
            "/knowledge/wiki",
        );
        bind_read_only(
            &mut command,
            views.root.join("wiki/daemon.toml"),
            "/opt/daemon.toml",
        );
        command
            .arg("--bind")
            .arg(views.root.join("wiki/socket"))
            .arg("/run/bench")
            .arg("--bind")
            .arg(&self.model_cache)
            .arg(MODEL_CACHE);
        command.args([
            "--setenv",
            "HF_HOME",
            MODEL_CACHE,
            "--setenv",
            "HALLOUMINATE_SOCKET",
            SOCKET,
            "--dir",
            WORKSPACE,
            "--chdir",
            WORKSPACE,
            "--",
            "/opt/hallouminate",
            "daemon",
            "--config",
            "/opt/daemon.toml",
        ]);
        let log = views.root.join("wiki/daemon.log");
        let mut daemon = Daemon(
            command
                .stdout(Stdio::null())
                .stderr(fs::File::create(&log)?)
                .spawn()
                .context("starting private wiki daemon")?,
        );
        let deadline = Instant::now() + DAEMON_START_TIMEOUT;
        while !views.socket().exists() {
            if let Some(status) = daemon.0.try_wait()? {
                bail!(
                    "private wiki daemon did not start ({status}): {}",
                    fs::read_to_string(&log).unwrap_or_default()
                );
            }
            if Instant::now() >= deadline {
                bail!(
                    "private wiki daemon did not start within {} s: {}",
                    DAEMON_START_TIMEOUT.as_secs(),
                    fs::read_to_string(&log).unwrap_or_default()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.preflight(views)?;
        Ok(daemon)
    }

    fn preflight(&self, views: &Views) -> anyhow::Result<()> {
        let status = self
            .arm(views, Arm::Baseline)
            .args(["--", "/usr/bin/test", "!", "-e", SOCKET])
            .status()
            .context("checking the baseline filesystem boundary")?;
        ensure!(
            status.success(),
            "baseline sandbox failed or its wiki socket is reachable"
        );

        let mut command = Command::new("/usr/bin/timeout");
        let wiki = self.arm(views, Arm::Wiki);
        let mut client = Daemon(
            command
                .env_clear()
                .args(["--kill-after=2s", "30s"])
                .arg(wiki.get_program())
                .args(wiki.get_args())
                .args(["--", "/opt/hallouminate", "serve"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .context("starting the wiki arm's MCP server for preflight")?,
        );
        let mut input = client.0.stdin.take().context("opening MCP stdin")?;
        let mut output = BufReader::new(client.0.stdout.take().context("opening MCP stdout")?);
        send(
            &mut input,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2024-11-05","capabilities":{},
                "clientInfo":{"name":"bench-run-preflight","version":"1"}
            }}),
        )?;
        receive(&mut output, 1)?;
        send(
            &mut input,
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )?;
        send(
            &mut input,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"list_corpora","arguments":{"cwd":WORKSPACE}
            }}),
        )?;
        let result = receive(&mut output, 2)?;
        let corpora = result["structuredContent"]["corpora"]
            .as_array()
            .context("MCP preflight returned no corpus list")?;
        ensure!(
            corpora.len() == 1
                && corpora[0]["name"] == CORPUS
                && corpora[0]["paths"] == json!(["/knowledge/wiki"]),
            "MCP preflight expected only {CORPUS} at /knowledge/wiki; got {corpora:?}"
        );
        send(
            &mut input,
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                "name":"read_markdown","arguments":{"cwd":WORKSPACE,"corpus":CORPUS,"path":views.page}
            }}),
        )?;
        let result = receive(&mut output, 3)?;
        let page = &result["structuredContent"];
        ensure!(
            page["corpus"] == CORPUS
                && page["content"].as_str()
                    == Some(
                        fs::read_to_string(views.root.join("wiki/data").join(&views.page))?
                            .as_str()
                    ),
            "MCP preflight could not read the expected frozen page {:?}",
            views.page
        );
        Ok(())
    }
}

fn bind_read_only(command: &mut Command, source: impl AsRef<OsStr>, dest: &str) {
    command.arg("--ro-bind").arg(source).arg(dest);
}

fn send(writer: &mut impl Write, value: Value) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *writer, &value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn receive(reader: &mut impl BufRead, id: u32) -> anyhow::Result<Value> {
    let mut line = String::new();
    while reader.read_line(&mut line)? != 0 {
        let value: Value = serde_json::from_str(&line).context("invalid preflight MCP reply")?;
        if value["id"] == id {
            ensure!(
                value["result"].is_object() && value["result"]["isError"] != true,
                "MCP preflight failed: {value}"
            );
            return Ok(value["result"].clone());
        }
        line.clear();
    }
    bail!("MCP server closed before preflight reply {id}")
}

pub(super) struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Views {
    pub(super) fn prepare(root: PathBuf, checkout: &Path, commit: &str) -> anyhow::Result<Self> {
        let archive = root.join("source.tar");
        fs::create_dir_all(&root)?;
        let output = Command::new("git")
            .current_dir(checkout)
            .args(["archive", "--format=tar", "-o"])
            .arg(&archive)
            .args([commit, "--", ".", ":(exclude,top).hallouminate"])
            .output()
            .context("archiving the pinned source commit")?;
        ensure!(
            output.status.success(),
            "git archive failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        for (name, config) in [("baseline", BASELINE_MCP), ("wiki", WIKI_MCP)] {
            let view = root.join(name);
            let source = view.join("source");
            fs::create_dir_all(&source)?;
            let output = Command::new("tar")
                .current_dir(source)
                .arg("-xf")
                .arg(&archive)
                .output()
                .context("extracting isolated pinned source")?;
            ensure!(
                output.status.success(),
                "tar failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            fs::write(view.join("mcp.json"), config)?;
        }
        let source_hash = agent_bench::blake3_file_hash(&archive)?;
        fs::remove_file(archive)?;

        let knowledge = checkout.join(".hallouminate");
        ensure!(
            fs::symlink_metadata(&knowledge)?.is_dir(),
            "wiki parent must be a real directory"
        );
        let frozen = knowledge.join("wiki");
        let copy = root.join("wiki/data");
        fs::create_dir(&copy)?;
        let mut files = Vec::new();
        copy_wiki(&frozen, &copy, Path::new(""), &mut files)?;
        files.sort();
        let page = files
            .iter()
            .find(|(path, _)| path.extension() == Some(OsStr::new("md")))
            .context("frozen wiki has no markdown page for MCP preflight")?
            .0
            .clone();
        let wiki_hash = agent_bench::provenance::fingerprint(&files)?;
        fs::write(root.join("wiki/daemon.toml"), DAEMON_CONFIG)?;
        fs::create_dir(root.join("wiki/socket"))?;
        Ok(Self {
            root,
            page,
            wiki_hash,
            source_hash,
        })
    }

    pub(super) fn fingerprints(&self) -> (&str, &str) {
        (&self.source_hash, &self.wiki_hash)
    }

    fn socket(&self) -> PathBuf {
        self.root.join("wiki/socket/daemon.sock")
    }
}

fn copy_wiki(
    source: &Path,
    dest: &Path,
    relative: &Path,
    files: &mut Vec<(PathBuf, String)>,
) -> anyhow::Result<()> {
    ensure!(
        fs::symlink_metadata(source)?.is_dir(),
        "wiki root is not a real directory: {}",
        source.display()
    );
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let relative = relative.join(&name);
        let target = dest.join(&name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            fs::create_dir(&target)?;
            copy_wiki(&entry.path(), &target, &relative, files)?;
        } else if kind.is_file() {
            let bytes = fs::read(entry.path())?;
            files.push((relative, blake3::hash(&bytes).to_hex().to_string()));
            fs::write(target, bytes)?;
        } else {
            bail!(
                "frozen wiki contains a symlink or special file: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}
