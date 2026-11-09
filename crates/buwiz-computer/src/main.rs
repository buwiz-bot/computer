use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use buwiz_computer::{BuwizComputer, ExecRequest, FilePutRequest, MkdirRequest};
use clap::{Parser, Subcommand};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "buwiz-computer",
    version,
    about = "Connect-only CLI for a running buwiz-computer guest"
)]
struct Cli {
    /// Published box-exec base URL (not /v1/info).
    #[arg(long, env = "BUWIZ_COMPUTER_EXEC_URL")]
    exec_url: String,
    /// Published box-host base URL (not /v1/info).
    #[arg(long, env = "BUWIZ_COMPUTER_HOST_URL")]
    host_url: String,
    /// Guest bearer token (BOX_TOKEN).
    #[arg(long, env = "BUWIZ_COMPUTER_TOKEN")]
    token: String,
    /// Bound on each request's whole round trip, in seconds. An exec with --timeout-ms and a
    /// cua wait get at least their own wait plus a margin.
    #[arg(
        long,
        env = "BUWIZ_COMPUTER_REQUEST_TIMEOUT_SECS",
        default_value_t = 120
    )]
    request_timeout_secs: u64,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// GET exec and host /v1/health
    Health,
    /// GET host /v1/ready
    Ready,
    /// GET host /v1/info (URLs in the body are container-local)
    Info,
    /// POST /v1/exec
    Exec {
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        timeout_ms: Option<u64>,
        #[arg(long)]
        stdin: Option<String>,
        #[arg(long)]
        stream: bool,
        #[arg(long)]
        detach: bool,
        #[arg(required = true, num_args = 1.., trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// GET /v1/exec/{id} (detached job)
    ExecStatus { id: String },
    /// DELETE /v1/exec/{id} (stop the exec and its process group)
    ExecCancel { id: String },
    #[command(subcommand)]
    Files(FilesCmd),
    /// GET host /v1/desktop
    Desktop,
    /// GET host /v1/screens (each screen's display, viewer port and state)
    Screens,
    /// GET host /v1/chrome
    Chrome,
    /// GET host /v1/egress
    Egress,
    /// GET host /v1/desktop/windows
    Windows,
    /// GET exec /v1/busy
    Busy,
    /// GET exec /v1/metrics
    Metrics,
    /// POST /v1/shutdown (exec, or --host)
    Shutdown {
        #[arg(long)]
        host: bool,
    },
    #[command(subcommand)]
    Cua(CuaCmd),
}

#[derive(Subcommand)]
enum FilesCmd {
    Get {
        path: String,
        #[arg(long)]
        encoding: Option<String>,
        /// Write GET /v1/files/raw bytes to a file (or stdout if omitted)
        #[arg(long)]
        raw: bool,
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    Put {
        path: String,
        #[arg(long, conflicts_with = "file")]
        content: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        encoding: Option<String>,
        /// PUT /v1/files/raw from --file
        #[arg(long)]
        raw: bool,
    },
    Delete {
        path: String,
        #[arg(long)]
        recursive: bool,
    },
    Mkdir {
        path: String,
        #[arg(long)]
        no_parents: bool,
    },
    Rename {
        from: String,
        to: String,
    },
}

#[derive(Subcommand)]
enum CuaCmd {
    /// POST /v1/cua/wait: until the screen settles, or changes with --change
    Wait {
        /// Still for this many ms
        #[arg(long)]
        idle: Option<u64>,
        /// Return at the first change
        #[arg(long)]
        change: bool,
        /// Count changes after this seq
        #[arg(long)]
        since: Option<u64>,
        /// Only watch x,y,w,h
        #[arg(long)]
        region: Option<String>,
        /// Give up after this many ms
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// POST /v1/cua/record/start
    RecordStart,
    /// POST /v1/cua/record/stop: saves the MP4 under the workspace
    RecordStop { id: u64 },
    Screenshot {
        /// Write raw image/png instead of JSON
        #[arg(long)]
        png: bool,
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    Click {
        #[arg(long)]
        x: i32,
        #[arg(long)]
        y: i32,
        #[arg(long)]
        button: Option<u8>,
    },
    DoubleClick {
        #[arg(long)]
        x: i32,
        #[arg(long)]
        y: i32,
        #[arg(long)]
        button: Option<u8>,
    },
    Move {
        #[arg(long)]
        x: i32,
        #[arg(long)]
        y: i32,
    },
    Drag {
        #[arg(long)]
        x1: i32,
        #[arg(long)]
        y1: i32,
        #[arg(long)]
        x2: i32,
        #[arg(long)]
        y2: i32,
        #[arg(long)]
        button: Option<u8>,
    },
    Press {
        #[arg(long)]
        x: i32,
        #[arg(long)]
        y: i32,
        #[arg(long)]
        button: Option<u8>,
    },
    Release {
        #[arg(long)]
        x: Option<i32>,
        #[arg(long)]
        y: Option<i32>,
        #[arg(long)]
        button: Option<u8>,
    },
    Type {
        #[arg(long)]
        text: String,
    },
    Key {
        #[arg(long)]
        key: String,
        /// tap (default), down, or up
        #[arg(long)]
        action: Option<String>,
    },
    Scroll {
        #[arg(long)]
        x: i32,
        #[arg(long)]
        y: i32,
        #[arg(long)]
        dx: i32,
        #[arg(long)]
        dy: i32,
    },
    /// POST /v1/cua/recipe — many CUA steps, one request
    Recipe {
        #[arg(long)]
        file: PathBuf,
    },
    /// POST /v1/cua/tree: the accessibility tree of the focused window, or the whole desktop
    Tree {
        /// focused_window (default) or desktop
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        max_depth: Option<u64>,
        #[arg(long)]
        max_nodes: Option<u64>,
        #[arg(long)]
        timeout_ms: Option<u64>,
        /// Also nodes that are neither showing nor visible
        #[arg(long)]
        include_hidden: bool,
        /// Keep the nameless containers compact mode leaves out
        #[arg(long)]
        no_compact: bool,
        /// Another of the box's screens (:N)
        #[arg(long)]
        display: Option<String>,
    },
    /// POST /v1/cua/element: press, click, focus or set_text on a node of the latest tree
    Element {
        /// The node's ref, from `cua tree`
        #[arg(long = "ref")]
        reference: String,
        /// press, click, focus or set_text
        #[arg(long)]
        action: String,
        /// The text, for set_text
        #[arg(long)]
        text: Option<String>,
        /// Another of the box's screens (:N)
        #[arg(long)]
        display: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let box_client = BuwizComputer::connect(cli.exec_url, cli.host_url, cli.token)?
        .with_timeout(std::time::Duration::from_secs(cli.request_timeout_secs));
    match cli.command {
        Commands::Health => {
            print_json(&json!({
                "exec": box_client.health_exec().await?,
                "host": box_client.health_host().await?,
            }))?;
        }
        Commands::Ready => print_json(&box_client.ready().await?)?,
        Commands::Info => print_json(&box_client.info().await?)?,
        Commands::Exec {
            cwd,
            timeout_ms,
            stdin,
            stream,
            detach,
            command,
        } => {
            let request = ExecRequest {
                command: json!(command),
                cwd,
                timeout_ms,
                env: None,
                stdin,
                detach: if detach { Some(true) } else { None },
                pty: None,
            };
            if stream {
                print!("{}", box_client.exec_stream(&request).await?);
            } else {
                print_json(&box_client.exec(&request).await?)?;
            }
        }
        Commands::ExecStatus { id } => print_json(&box_client.exec_status(&id).await?)?,
        Commands::ExecCancel { id } => print_json(&box_client.exec_cancel(&id).await?)?,
        Commands::Files(cmd) => match cmd {
            FilesCmd::Get {
                path,
                encoding,
                raw,
                output,
            } => {
                if raw {
                    let bytes = box_client.files_get_raw(&path).await?;
                    if let Some(path) = output {
                        std::fs::write(&path, &bytes)
                            .with_context(|| format!("write {}", path.display()))?;
                    } else {
                        std::io::stdout().write_all(&bytes)?;
                    }
                } else {
                    print_json(&box_client.files_get(&path, encoding.as_deref()).await?)?;
                }
            }
            FilesCmd::Put {
                path,
                content,
                file,
                encoding,
                raw,
            } => {
                if raw {
                    let file = file.context("--file is required with --raw")?;
                    let bytes =
                        std::fs::read(&file).with_context(|| format!("read {}", file.display()))?;
                    print_json(&box_client.files_put_raw(&path, bytes).await?)?;
                } else {
                    let content = if let Some(file) = file {
                        std::fs::read_to_string(&file)
                            .with_context(|| format!("read {}", file.display()))?
                    } else {
                        content.context("--content or --file is required")?
                    };
                    print_json(
                        &box_client
                            .files_put(&FilePutRequest {
                                path,
                                content,
                                encoding,
                                create_dirs: Some(true),
                            })
                            .await?,
                    )?;
                }
            }
            FilesCmd::Delete { path, recursive } => {
                print_json(&box_client.files_delete(&path, recursive).await?)?;
            }
            FilesCmd::Mkdir { path, no_parents } => {
                print_json(
                    &box_client
                        .files_mkdir(&MkdirRequest {
                            path,
                            parents: Some(!no_parents),
                        })
                        .await?,
                )?;
            }
            FilesCmd::Rename { from, to } => {
                print_json(&box_client.files_rename(&from, &to).await?)?;
            }
        },
        Commands::Desktop => print_json(&box_client.desktop().await?)?,
        Commands::Screens => print_json(&box_client.screens().await?)?,
        Commands::Chrome => print_json(&box_client.chrome().await?)?,
        Commands::Egress => print_json(&box_client.egress().await?)?,
        Commands::Windows => print_json(&box_client.windows().await?)?,
        Commands::Busy => print_json(&box_client.busy().await?)?,
        Commands::Metrics => print_json(&box_client.metrics().await?)?,
        Commands::Shutdown { host } => print_json(&box_client.shutdown(host).await?)?,
        Commands::Cua(cmd) => match cmd {
            CuaCmd::Wait {
                idle,
                change,
                since,
                region,
                timeout,
            } => {
                let body = serde_json::json!({
                    "idle": idle,
                    "change": change,
                    "since": since,
                    "region": region,
                    "timeout": timeout,
                });
                print_json(&box_client.cua_wait(body).await?)?;
            }
            CuaCmd::RecordStart => print_json(&box_client.cua_record_start().await?)?,
            CuaCmd::RecordStop { id } => print_json(&box_client.cua_record_stop(id).await?)?,
            CuaCmd::Screenshot { png, output } => {
                if png {
                    let bytes = box_client.screenshot_png().await?;
                    if let Some(path) = output {
                        std::fs::write(&path, &bytes)
                            .with_context(|| format!("write {}", path.display()))?;
                    } else {
                        std::io::stdout().write_all(&bytes)?;
                    }
                } else {
                    let shot = box_client.screenshot().await?;
                    if let Some(path) = output {
                        let raw = serde_json::to_vec_pretty(&shot)?;
                        std::fs::write(&path, raw)
                            .with_context(|| format!("write {}", path.display()))?;
                    } else {
                        print_json(&shot)?;
                    }
                }
            }
            CuaCmd::Click { x, y, button } => {
                print_json(&box_client.click(x, y, button).await?)?;
            }
            CuaCmd::DoubleClick { x, y, button } => {
                print_json(&box_client.double_click(x, y, button).await?)?;
            }
            CuaCmd::Move { x, y } => print_json(&box_client.move_pointer(x, y).await?)?,
            CuaCmd::Drag {
                x1,
                y1,
                x2,
                y2,
                button,
            } => print_json(&box_client.drag(x1, y1, x2, y2, button).await?)?,
            CuaCmd::Press { x, y, button } => {
                print_json(&box_client.press(x, y, button).await?)?;
            }
            CuaCmd::Release { x, y, button } => {
                print_json(&box_client.release(x, y, button, None).await?)?;
            }
            CuaCmd::Type { text } => print_json(&box_client.type_text(&text).await?)?,
            CuaCmd::Key { key, action } => {
                print_json(&box_client.key_action(&key, action.as_deref()).await?)?;
            }
            CuaCmd::Scroll { x, y, dx, dy } => {
                print_json(&box_client.scroll(x, y, dx, dy).await?)?;
            }
            CuaCmd::Recipe { file } => {
                let raw = std::fs::read_to_string(&file)
                    .with_context(|| format!("read {}", file.display()))?;
                let request: serde_json::Value = serde_json::from_str(&raw)
                    .with_context(|| format!("parse {}", file.display()))?;
                print_json(&box_client.recipe(&request).await?)?;
            }
            CuaCmd::Tree {
                scope,
                max_depth,
                max_nodes,
                timeout_ms,
                include_hidden,
                no_compact,
                display,
            } => {
                let on = display.map_or_else(|| box_client.clone(), |d| box_client.on(&d));
                let mut body = json!({});
                if let Some(scope) = scope {
                    body["scope"] = json!(scope);
                }
                if let Some(n) = max_depth {
                    body["max_depth"] = json!(n);
                }
                if let Some(n) = max_nodes {
                    body["max_nodes"] = json!(n);
                }
                if let Some(ms) = timeout_ms {
                    body["timeout_ms"] = json!(ms);
                }
                if include_hidden {
                    body["include_hidden"] = json!(true);
                }
                if no_compact {
                    body["compact"] = json!(false);
                }
                print_json(&on.cua_tree(body).await?)?;
            }
            CuaCmd::Element {
                reference,
                action,
                text,
                display,
            } => {
                let on = display.map_or_else(|| box_client.clone(), |d| box_client.on(&d));
                print_json(&on.cua_element(&reference, &action, text.as_deref()).await?)?;
            }
        },
    }
    Ok(())
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer_pretty(std::io::stdout(), value)?;
    println!();
    Ok(())
}
