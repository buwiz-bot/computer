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
    /// Bound on each request's whole round trip, in seconds. An exec with --timeout-ms, a cua
    /// wait and a cua tree get at least their own wait plus a margin; a file read or write gets
    /// time for its size too.
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
    /// /v1/browser/*: a screen's Chromium over its DevTools pipe, through ultra-instinct
    Browser {
        /// Another of the box's screens (:N)
        #[arg(long, global = true)]
        display: Option<String>,
        #[command(subcommand)]
        cmd: BrowserCmd,
    },
}

#[derive(Subcommand)]
enum BrowserCmd {
    /// POST /v1/browser/snap: the current tab's controls, each with an @eN ref
    Snap {
        /// The most elements returned (1 to 1000, default 200)
        #[arg(long)]
        max_nodes: Option<u64>,
    },
    /// POST /v1/browser/act: click, type, select, key, scroll or focus an element by ref
    Act {
        /// @eN from the latest snap
        #[arg(long = "ref")]
        reference: String,
        /// click, type, select, key, scroll or focus
        #[arg(long)]
        action: String,
        /// type: the text; select: the option; key: the key
        #[arg(long)]
        text: Option<String>,
        /// The snapshot the ref is from
        #[arg(long)]
        snapshot: Option<u64>,
        /// The token a confirm_required answer gave, once a person approved its label
        #[arg(long)]
        confirm: Option<String>,
        /// A host the page must be on (repeatable; *.example.com for its subdomains)
        #[arg(long = "allow-domain")]
        allow_domains: Vec<String>,
    },
    /// POST /v1/browser/navigate: the current tab to an http(s) URL
    Navigate {
        url: String,
        #[arg(long = "allow-domain")]
        allow_domains: Vec<String>,
    },
    /// POST /v1/browser/tabs: list, open, close or switch tabs
    Tabs {
        /// list, open, close or switch
        op: String,
        /// close, switch: the tab
        #[arg(long)]
        tab_id: Option<String>,
        /// open: the page
        #[arg(long)]
        url: Option<String>,
        #[arg(long = "allow-domain")]
        allow_domains: Vec<String>,
    },
    /// POST /v1/browser/wait: until the page shows text, an address, a control, or loads
    Wait {
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        control: Option<String>,
        #[arg(long)]
        gone: Option<String>,
        /// domcontentloaded or load
        #[arg(long)]
        load: Option<String>,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// POST /v1/browser/shot: the current tab's viewport as a PNG
    Shot {
        /// Write the PNG here instead of printing the JSON
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// POST /v1/browser/run: Instinct runs a goal until done, blocked or abstained
    Run {
        goal: String,
        #[arg(long)]
        max_steps: Option<u32>,
        #[arg(long)]
        timeout_ms: Option<u64>,
        #[arg(long = "allow-domain")]
        allow_domains: Vec<String>,
        /// The token a blocked run gave, once a person approved its confirm_label
        #[arg(long)]
        confirm: Option<String>,
    },
    /// POST /v1/browser/state: save or load this screen's login
    State {
        /// save or load
        op: String,
        /// Which saved login (default `default`)
        #[arg(long)]
        name: Option<String>,
    },
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
        Commands::Browser { display, cmd } => {
            let on = display.map_or_else(|| box_client.clone(), |d| box_client.on(&d));
            browser(&on, cmd).await?;
        }
    }
    Ok(())
}

/// `allow_domains` in `body`, when any were given.
fn with_allow(mut body: serde_json::Value, allow_domains: Vec<String>) -> serde_json::Value {
    if !allow_domains.is_empty() {
        body["allow_domains"] = json!(allow_domains);
    }
    body
}

/// `body` without its null fields: the guest takes an absent field as its default.
fn compact(mut body: serde_json::Value) -> serde_json::Value {
    if let Some(map) = body.as_object_mut() {
        map.retain(|_, v| !v.is_null());
    }
    body
}

async fn browser(on: &BuwizComputer, cmd: BrowserCmd) -> Result<()> {
    let answer = match cmd {
        BrowserCmd::Snap { max_nodes } => {
            on.browser_snap(compact(json!({ "max_nodes": max_nodes })))
                .await?
        }
        BrowserCmd::Act {
            reference,
            action,
            text,
            snapshot,
            confirm,
            allow_domains,
        } => {
            let body = json!({
                "ref": reference, "action": action, "text": text,
                "snapshot": snapshot, "confirm": confirm,
            });
            on.browser_act(with_allow(compact(body), allow_domains))
                .await?
        }
        BrowserCmd::Navigate { url, allow_domains } => {
            on.browser_navigate(with_allow(json!({ "url": url }), allow_domains))
                .await?
        }
        BrowserCmd::Tabs {
            op,
            tab_id,
            url,
            allow_domains,
        } => {
            let body = compact(json!({ "op": op, "tab_id": tab_id, "url": url }));
            on.browser_tabs(with_allow(body, allow_domains)).await?
        }
        BrowserCmd::Wait {
            text,
            url,
            control,
            gone,
            load,
            timeout_ms,
        } => {
            let body = json!({
                "text": text, "url": url, "control": control, "gone": gone,
                "load": load, "timeout_ms": timeout_ms,
            });
            on.browser_wait(compact(body)).await?
        }
        BrowserCmd::Shot { output } => {
            let shot = on.browser_shot().await?;
            if let Some(path) = output {
                use base64::Engine as _;
                let png = base64::engine::general_purpose::STANDARD
                    .decode(shot["data"].as_str().unwrap_or_default())
                    .context("the shot's data isn't base64")?;
                std::fs::write(&path, png).with_context(|| format!("write {}", path.display()))?;
                return Ok(());
            }
            shot
        }
        BrowserCmd::Run {
            goal,
            max_steps,
            timeout_ms,
            allow_domains,
            confirm,
        } => {
            let body = json!({
                "goal": goal, "max_steps": max_steps, "timeout_ms": timeout_ms,
                "confirm": confirm,
            });
            on.browser_run(with_allow(compact(body), allow_domains))
                .await?
        }
        BrowserCmd::State { op, name } => {
            on.browser_state(compact(json!({ "op": op, "name": name })))
                .await?
        }
    };
    print_json(&answer)
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer_pretty(std::io::stdout(), value)?;
    println!();
    Ok(())
}
