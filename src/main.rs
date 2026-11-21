mod desktop;
mod layout;
mod notifications;
mod order;
mod slide;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use desktop::Desktop;
use layout::{Layout, SavedLayout};
use notifications::Notifier;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use slide::SlideExperiment;
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::{
        fs::FileTypeExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const APP: &str = "cosmic-project-workspaces";
const MAX_MESSAGE: u64 = 64 * 1024;
type Reply = std::result::Result<String, String>;

#[derive(Parser)]
#[command(
    about = "Keyboard-driven project columns for COSMIC workspaces",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Serialize, Deserialize)]
enum Command {
    /// Run the helper in the foreground
    Serve {
        /// Don't show project-name banners when switching projects
        #[arg(long)]
        no_notifications: bool,
        /// Temporarily toggle COSMIC's global slide axis for each cpw move
        #[arg(long)]
        experimental_slide: bool,
        /// Project for workspaces that haven't been assigned
        #[arg(long, default_value = "Work", value_name = "NAME")]
        default_project: String,
    },
    /// List project columns, their rows, and the active workspace
    Status {
        /// Show labeled project columns side by side
        #[arg(long)]
        grid: bool,
    },
    /// Match COSMIC's workspace order to the project columns
    SyncOrder {
        /// Show the proposed order without changing COSMIC
        #[arg(long)]
        dry_run: bool,
    },
    /// Assign the active workspace, creating the project if necessary
    Assign {
        project: String,
        /// Put this workspace at the top instead of the bottom
        #[arg(long)]
        top: bool,
    },
    /// Return the active workspace to the default project
    Unassign,
    /// Remove a project and return its workspaces to the default project
    Forget { project: String },
    /// Return to the last-used workspace in a named project
    Go { project: String },
    /// Switch left to the neighboring project's last-used workspace
    Left,
    /// Switch right to the neighboring project's last-used workspace
    Right,
    /// Move up within the active project, borrowing an empty row at the top
    Up,
    /// Move down within the active project, borrowing an empty row at the bottom
    Down,
}

fn main() -> Result<()> {
    let command = Cli::parse().command;
    let runtime = env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    let runtime = Path::new(&runtime);
    match command {
        Command::Serve {
            no_notifications,
            experimental_slide,
            default_project,
        } => serve(
            runtime,
            no_notifications,
            experimental_slide,
            default_project,
        ),
        other => {
            let stream = UnixStream::connect(socket_path(runtime))
                .context("could not contact the helper; start it with `cpw serve`")?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            send(&stream, &other)?;
            let response: Reply = receive(&stream).context("read the helper's reply")?;
            println!("{}", response.map_err(anyhow::Error::msg)?);
            Ok(())
        }
    }
}

fn serve(
    runtime: &Path,
    no_notifications: bool,
    experimental_slide: bool,
    default_project: String,
) -> Result<()> {
    layout::validate_name(&default_project).context("invalid --default-project")?;
    let socket = CommandSocket::bind(runtime)?;
    let (mut file, layout) = StateFile::load()?;
    let stopping = Arc::new(AtomicBool::new(false));
    let slide = if experimental_slide {
        let slide = SlideExperiment::new(&config_home()?)?;
        for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            signal_hook::flag::register(signal, Arc::clone(&stopping))?;
        }
        eprintln!(
            "Experimental slide enabled: cpw will temporarily change COSMIC's workspace layout."
        );
        Some(slide)
    } else {
        None
    };
    let mut desktop = Desktop::connect(layout, slide, default_project)?;
    file.save(desktop.snapshot())?;
    let notifier = if no_notifications {
        None
    } else {
        Notifier::start()
            .inspect_err(|error| eprintln!("Project notifications unavailable: {error}"))
            .ok()
    };
    println!("Project workspace helper is ready. Ctrl+C to stop.");
    while !stopping.load(Ordering::Relaxed) {
        let accept_command = desktop.wait(&socket.listener)?;
        file.save(desktop.snapshot())?;
        if stopping.load(Ordering::Relaxed) {
            break;
        }
        if accept_command {
            let (stream, _) = socket.listener.accept().context("accept a command")?;
            if let Err(error) = serve_client(stream, &mut desktop, &mut file) {
                eprintln!("Command connection: {error:#}");
            }
        }
        if let (Some(project), Some(notifier)) = (desktop.take_project_notification(), &notifier) {
            notifier.show(project);
        }
    }
    Ok(())
}

fn serve_client(stream: UnixStream, desktop: &mut Desktop, file: &mut StateFile) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    let reply: Reply = receive(&stream)
        .and_then(|command| desktop.execute(command))
        .and_then(|response| {
            file.save(desktop.snapshot())?;
            Ok(response)
        })
        .map_err(|error| format!("{error:#}"));
    send(&stream, &reply)
}

fn send(mut stream: &UnixStream, message: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(message)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    Ok(())
}

fn receive<T: DeserializeOwned>(stream: &UnixStream) -> Result<T> {
    let mut bytes = Vec::new();
    BufReader::new(stream.take(MAX_MESSAGE + 1)).read_until(b'\n', &mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_MESSAGE && bytes.ends_with(b"\n"),
        "invalid command message"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

fn socket_path(runtime: &Path) -> PathBuf {
    runtime.join(format!("{APP}.sock"))
}

fn config_home() -> Result<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("neither XDG_CONFIG_HOME nor HOME is set")
}

struct CommandSocket {
    listener: UnixListener,
    path: PathBuf,
    _lock: File,
}

impl CommandSocket {
    fn bind(runtime: &Path) -> Result<Self> {
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(runtime.join(format!("{APP}.lock")))?;
        lock.try_lock()
            .context("another project workspace helper may already be running")?;
        let path = socket_path(runtime);
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                ensure!(
                    meta.file_type().is_socket(),
                    "{} is not a socket",
                    path.display()
                );
                fs::remove_file(&path)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener =
            UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
        Ok(Self {
            listener,
            path,
            _lock: lock,
        })
    }
}

impl Drop for CommandSocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct StateFile {
    path: PathBuf,
    saved: SavedLayout,
}

impl StateFile {
    fn load() -> Result<(Self, Layout)> {
        let path = config_home()?.join(APP).join("layout.json");
        let result = fs::read(&path);
        let layout = match result {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .context("invalid project layout")
                .and_then(Layout::restore),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Layout::default()),
            Err(error) => Err(error.into()),
        }
        .with_context(|| format!("load {}", path.display()))?;
        let saved = layout.snapshot();
        Ok((Self { path, saved }, layout))
    }

    fn save(&mut self, snapshot: SavedLayout) -> Result<()> {
        if self.saved != snapshot {
            self.write(&snapshot)
                .with_context(|| format!("save {}", self.path.display()))?;
            self.saved = snapshot;
        }
        Ok(())
    }

    fn write(&self, snapshot: &SavedLayout) -> Result<()> {
        let directory = self
            .path
            .parent()
            .context("project config path has no parent")?;
        fs::create_dir_all(directory)?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer_pretty(&mut temporary, snapshot)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path)?;
        Ok(())
    }
}
