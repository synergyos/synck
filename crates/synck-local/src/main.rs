use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anstyle::{AnsiColor, Color, Style};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use synck_local::{
    Change, ChangeKind, CommitEvent, CommitOutcome, DetailedEventLogEntry, DetailedLogEntry,
    EventHistoryCommit, HiddenFilesMode, HistoryCommit, LocalError, LogEntry, MonitorStatus,
    Repository, RepositoryInfo, TEXT_DIFF_MAX_BYTES, TextDiffEntry, TextDiffKind,
};

const CLEAN_STATUS_MESSAGE: &str = "nothing to commit, working tree clean";

mod cli_json;

const CONTEXT_CHANGE_LIMIT: usize = 100;
const CONTEXT_COMMIT_LIMIT: usize = 5;
const INSTRUCTIONS: &str = include_str!("instructions.txt");
const ADDED_STYLE: Style = Style::new()
    .fg_color(Some(Color::Ansi(AnsiColor::BrightBlue)))
    .bold();
const MODIFIED_STYLE: Style = Style::new()
    .fg_color(Some(Color::Ansi(AnsiColor::BrightYellow)))
    .bold();
const DELETED_STYLE: Style = Style::new()
    .fg_color(Some(Color::Ansi(AnsiColor::BrightRed)))
    .bold();
const HEALTHY_STYLE: Style = Style::new()
    .fg_color(Some(Color::Ansi(AnsiColor::BrightGreen)))
    .bold();
const COMMIT_STYLE: Style = Style::new().bold();

/// `synck log | head` closes stdout early. Rust ignores SIGPIPE, so the next
/// print panics with a broken pipe. Treat that like other command-line tools:
/// stop quietly with the conventional SIGPIPE status. Only stdout/stderr
/// writes are affected; every repository write has already finished or will
/// be recovered by the next command.
fn exit_quietly_when_output_is_closed() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        let closed_pipe = message.contains("os error 32") || message.contains("os error 232");
        if message.starts_with("failed printing to std") && closed_pipe {
            std::process::exit(141);
        }
        previous(info);
    }));
}

#[derive(Debug, Parser)]
#[command(
    name = "synck",
    version,
    about = "Version control for knowledge work: files, email and chats with complete, verifiable history"
)]
struct Cli {
    /// Emit one versioned JSON document for supported commands.
    #[arg(long, global = true)]
    json: bool,
    /// Ownership token returned by lock acquire; repeat for multiple reservations.
    #[arg(long, global = true)]
    lock_token: Vec<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum LockCommand {
    /// Reserve clean existing paths or absent paths atomically.
    Acquire {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Agent/session label for diagnostics; ownership is proved by the token.
        #[arg(long)]
        owner: String,
        #[arg(long, default_value_t = 600)]
        ttl: u64,
    },
    /// Extend an unexpired lease with the exact token returned by acquire.
    Renew {
        token: String,
        #[arg(long, default_value_t = 600)]
        ttl: u64,
    },
    /// Release exactly this token's reservations, preserving all working files.
    Release { token: String },
    /// List reservations, including expired ones; never reveals ownership tokens.
    List,
    /// Explicitly remove overlapping reservations after inspecting abandoned work.
    Break {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Reserve local paths for cooperating agents, including new files.
    Lock {
        #[command(subcommand)]
        command: LockCommand,
    },
    /// Copy, verify, check out or write to a repository by its directory.
    Repository {
        #[command(subcommand)]
        command: PortableCommand,
    },
    /// Read files, email, chats and other items, at any point in history.
    Item {
        #[command(subcommand)]
        command: ItemCommand,
    },
    #[command(name = "__monitor-server", hide = true)]
    MonitorServer {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        repo_id: String,
    },
    /// Explain local Synck workflows.
    Instructions,
    /// Create a complete repository in a directory and commit its default .synckignore.
    Init {
        /// Directory to track; existing files stay uncommitted.
        #[arg(default_value = ".")]
        directory: PathBuf,
        /// Name recorded on your commits.
        #[arg(long)]
        author: String,
        /// Rename non-portable names and report every change.
        #[arg(long)]
        fix_names: bool,
        /// CHUNKED (compact, the default) or WHOLE_FILE (one plain file per version).
        #[arg(long, default_value = "CHUNKED", value_parser = parse_storage_mode)]
        storage_mode: synck_local::StorageMode,
    },
    /// Show repository identity, format, author, and current head.
    Info,
    /// Summarize repository identity, current changes, and recent commits.
    Context,
    /// Read or update local repository configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Show the complete working-tree delta.
    Status,
    /// Manage the optional per-repository filesystem monitor.
    Monitor {
        #[command(subcommand)]
        command: MonitorCommand,
    },
    /// Commit the complete delta or only the selected paths.
    Commit {
        /// Repair non-portable names throughout the observed checkout before committing.
        #[arg(long)]
        fix_names: bool,
        /// Commit message.
        #[arg(short = 'm', long)]
        message: Option<String>,
        /// Commit only changes under these paths.
        paths: Vec<PathBuf>,
    },
    /// Append a new commit that reverses one accepted commit.
    Revert {
        /// Commit to reverse: HEAD, HEAD~N, a sequence number or a full commit ID.
        revision: String,
        /// Commit message.
        #[arg(short = 'm', long)]
        message: String,
    },
    /// Move a tracked item and preserve its identity.
    Mv {
        /// Tracked file or folder to move.
        source: PathBuf,
        /// New path; its parent folder must already be tracked.
        destination: PathBuf,
        /// Commit message.
        #[arg(short = 'm', long)]
        message: String,
    },
    /// Show sequenced commits newest first.
    Log {
        /// Number of commits to show.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// Print each commit on one line.
        #[arg(long)]
        oneline: bool,
        /// Show exact authoritative events instead of semantic change rows.
        #[arg(long, conflicts_with = "oneline")]
        events: bool,
    },
    /// Show one commit and its knowledge-work delta.
    Show {
        /// HEAD, HEAD~N, a sequence number or a full commit ID.
        revision: String,
        /// Show exact authoritative events instead of semantic change rows.
        #[arg(long)]
        events: bool,
    },
    /// Follow one live item across its committed history.
    History { path: PathBuf },
    /// Compare the repository structure at two accepted revisions.
    Compare { from: String, to: String },
    /// Show unified UTF-8 text differences for the worktree or two revisions.
    Diff {
        #[arg(value_name = "REVISION", num_args = 0..=2)]
        revisions: Vec<String>,
    },
    /// List items currently in the repository trash.
    Deleted,
    /// Recover a trashed item while preserving its identity and history.
    Recover {
        /// Path the item had when it was deleted, or its item ID from `deleted`.
        #[arg(value_name = "PATH_OR_ID")]
        item_id: String,
        /// Commit message.
        #[arg(short = 'm', long)]
        message: String,
    },
    /// Show committed content versions of a live file.
    Versions { path: PathBuf },
    /// Manage named checkpoints on a live file.
    Version {
        #[command(subcommand)]
        command: VersionCommand,
    },
    /// Replace a live file with committed bytes.
    Restore {
        path: PathBuf,
        /// Version to restore, as listed by `versions`; the latest by default.
        #[arg(long)]
        item_seq: Option<i64>,
    },
    /// Discard the complete canonical working-tree delta.
    RestoreTree,
    /// Copy committed file bytes to a new destination.
    Export {
        path: PathBuf,
        /// New file to write; it must not exist.
        destination: PathBuf,
        /// Version to export, as listed by `versions`; the latest by default.
        #[arg(long)]
        item_seq: Option<i64>,
    },
    /// Export a folder or complete repository tree at an accepted commit.
    ExportTree {
        /// HEAD, HEAD~N, a sequence number or a full commit ID.
        revision: String,
        /// New directory to write; it must not exist.
        destination: PathBuf,
        /// Export only this folder, by its path at that revision.
        #[arg(long)]
        folder: Option<PathBuf>,
    },
    /// Verify history, current state and every stored byte.
    Fsck {
        /// Rebuild the disposable historical state index before verification.
        #[arg(long)]
        rebuild_state_index: bool,
    },
    /// Report referenced and unreachable stored content.
    Storage {
        #[command(subcommand)]
        command: Option<StorageCommand>,
    },
    /// Remove stored content that no commit refers to.
    Gc,
    /// Write a verified copy of this repository to a new directory.
    Archive {
        /// New directory to write; it must not exist.
        destination: PathBuf,
        /// Storage layout of the copy; the repository's own by default.
        #[arg(long, value_parser = parse_storage_mode)]
        storage_mode: Option<synck_local::StorageMode>,
    },
}

#[derive(Debug, Subcommand)]
enum PortableCommand {
    /// Same as `synck init`.
    Init {
        /// Directory to track; existing files stay uncommitted.
        #[arg(default_value = ".")]
        destination: PathBuf,
        /// Name recorded on your commits.
        #[arg(long)]
        author: String,
        /// Rename non-portable names and report every change.
        #[arg(long)]
        fix_names: bool,
        /// CHUNKED (compact, the default) or WHOLE_FILE (one plain file per version).
        #[arg(long, default_value = "CHUNKED", value_parser = parse_storage_mode)]
        storage_mode: synck_local::StorageMode,
    },
    /// Write a copy's files and folders to disk so you can edit and commit them.
    Checkout {
        /// Repository directory; it must contain nothing but .synck.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Name recorded on your commits.
        #[arg(long)]
        author: String,
        /// Literal repository-relative subtree; repeat for several. Omit for all.
        #[arg(long = "include")]
        paths: Vec<String>,
    },
    /// Change which paths a checkout has on disk; commit your changes first.
    CheckoutSelection {
        /// Repository directory.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Literal repository-relative subtree; repeat for several.
        #[arg(
            long = "include",
            required_unless_present = "all",
            conflicts_with = "all"
        )]
        paths: Vec<String>,
        /// Put every file and folder on disk.
        #[arg(long)]
        all: bool,
    },
    /// Make a verified copy of a repository in a new directory, as a follower.
    Import {
        /// Repository to copy; it is never changed.
        source: PathBuf,
        /// New directory to write; it must not exist.
        destination: PathBuf,
        /// Storage layout of the copy; the source's own by default.
        #[arg(long, value_parser = parse_storage_mode)]
        storage_mode: Option<synck_local::StorageMode>,
    },
    /// Verify history, current state and every stored byte.
    Verify {
        /// Repository directory.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// List sealed commits newest first, with each commit's events.
    Log {
        /// Repository directory.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Commits per page, from 1 to 512.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Show commits before this repository sequence; use next_before.
        #[arg(long)]
        before: Option<i64>,
    },
    /// Make this copy the writer that succeeds a retired source.
    Activate {
        /// Repository directory.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Name recorded on your commits.
        #[arg(long)]
        author: String,
        /// Acknowledge that the former writer has been retired; this does not fence it.
        #[arg(long, required = true)]
        source_retired: bool,
    },
    /// Write a verified copy of a repository to a new directory, as a follower.
    Archive {
        /// New directory to write; it must not exist.
        destination: PathBuf,
        /// Repository to copy; it is never changed.
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Storage layout of the copy; the source's own by default.
        #[arg(long, value_parser = parse_storage_mode)]
        storage_mode: Option<synck_local::StorageMode>,
    },
    /// Complete an accepted write that reported pending recovery; never repeat the write.
    Finalize {
        /// Repository directory.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Create a FILE item from a local file's bytes.
    CreateFile {
        /// Name of the new file in the repository.
        name: String,
        /// Local file to read.
        source: PathBuf,
        /// Folder item ID to create it in; the top level by default.
        #[arg(long)]
        parent: Option<String>,
        #[command(flatten)]
        write: WriteArgs,
        /// Repository directory.
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Record new content for an existing FILE item, keeping its identity.
    SaveFile {
        /// FILE item ID.
        item_id: String,
        /// Local file to read.
        source: PathBuf,
        /// Refuse with CONFLICT unless the current content is this blob ID.
        #[arg(long)]
        base_content: Option<String>,
        #[command(flatten)]
        write: WriteArgs,
        /// Repository directory.
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Create a chat with a title.
    CreateChat {
        /// Title people see; it does not need to be unique.
        title: String,
        /// Repository directory.
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Add a message to a chat.
    AppendChat {
        /// CHAT item ID.
        item_id: String,
        /// UTF-8 text file holding the message.
        body_file: PathBuf,
        /// File to attach; repeat for several.
        #[arg(long = "attachment")]
        attachments: Vec<PathBuf>,
        /// Repository directory.
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
}

/// Attribution and idempotency for direct FILE writes.
#[derive(Debug, clap::Args)]
struct WriteArgs {
    /// Commit message.
    #[arg(short = 'm', long)]
    message: Option<String>,
    /// Author of this revision; the repository author stays the committer.
    #[arg(long)]
    author: Option<String>,
    /// When the change happened, as an RFC 3339 timestamp.
    #[arg(long)]
    occurred_at: Option<String>,
    /// Canonical UUID naming this write; a repeat returns the original commit.
    #[arg(long)]
    operation_id: Option<String>,
}

impl WriteArgs {
    fn options(&self) -> Result<synck_local::portable::WriteOptions, LocalError> {
        let occurred_at = self
            .occurred_at
            .as_deref()
            .map(|value| {
                chrono::DateTime::parse_from_rfc3339(value)
                    .map(|time| time.with_timezone(&chrono::Utc))
                    .map_err(|error| {
                        LocalError::Invalid(format!("--occurred-at must be RFC 3339: {error}"))
                    })
            })
            .transpose()?;
        let operation_id = self
            .operation_id
            .as_deref()
            .map(|value| match uuid::Uuid::parse_str(value) {
                Ok(id) if id.to_string() == value => Ok(id),
                _ => Err(LocalError::Invalid(
                    "--operation-id must be a canonical lowercase UUID".to_string(),
                )),
            })
            .transpose()?;
        Ok(synck_local::portable::WriteOptions {
            message: self.message.clone(),
            author: self.author.clone(),
            occurred_at,
            operation_id,
        })
    }
}

#[derive(Debug, Subcommand)]
enum ItemCommand {
    /// Write a file's content, or an email's original message, to a new file.
    Export {
        /// FILE or EMAIL item ID.
        item_id: String,
        /// New file to write; it must not exist.
        destination: PathBuf,
        /// Read as of this sealed repository sequence.
        #[arg(long)]
        through: Option<i64>,
        /// Repository directory.
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Read one item as JSON, including messages, attachments and refs.
    Show {
        /// Item ID.
        item_id: String,
        /// Read as of this sealed repository sequence.
        #[arg(long)]
        through: Option<i64>,
        /// Repository directory.
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// List live items at the top level or in one folder, as JSON.
    List {
        /// Folder item ID; the top level by default.
        #[arg(long)]
        parent: Option<String>,
        /// The next_cursor value from the previous page.
        #[arg(long)]
        after: Option<String>,
        /// Read as of this sealed repository sequence.
        #[arg(long)]
        through: Option<i64>,
        /// Items per page, from 1 to 512.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Repository directory.
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Show the author used for future commits.
    Author,
    /// Set the author used for future commits.
    SetAuthor { author: String },
    /// Show whether untracked hidden files are ignored or tracked.
    HiddenFiles,
    /// Choose whether untracked hidden files are ignored or tracked.
    SetHiddenFiles { mode: HiddenFilesArgument },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum HiddenFilesArgument {
    Ignore,
    Track,
}

impl From<HiddenFilesArgument> for HiddenFilesMode {
    fn from(value: HiddenFilesArgument) -> Self {
        match value {
            HiddenFilesArgument::Ignore => Self::Ignore,
            HiddenFilesArgument::Track => Self::Track,
        }
    }
}

#[derive(Debug, Subcommand)]
enum MonitorCommand {
    /// Start the repository monitor if it is not running.
    Start,
    /// Show whether the repository monitor is running.
    Status,
    /// Stop the repository monitor if it is running.
    Stop,
}

#[derive(Debug, Subcommand)]
enum StorageCommand {
    /// Convert whole-file storage to compact chunked storage in place.
    Optimize,
}

#[derive(Debug, Subcommand)]
enum VersionCommand {
    /// List live named versions in display order.
    List {
        path: PathBuf,
        /// Include permanently tombstoned labels.
        /// Include removed (tombstoned) versions.
        #[arg(long)]
        all: bool,
    },
    /// Name the current or selected FILE content revision.
    Create {
        path: PathBuf,
        /// File version to name, as listed by `versions`; the latest by default.
        #[arg(long)]
        item_seq: Option<i64>,
        /// Create a major checkpoint; the default is minor.
        /// Make it a major version; minor by default.
        #[arg(long)]
        major: bool,
        /// Optional description.
        #[arg(long)]
        description: Option<String>,
        /// Commit message.
        #[arg(short = 'm', long)]
        message: String,
    },
    /// Replace a named version's description; an empty value clears it.
    SetDescription {
        path: PathBuf,
        version_id: String,
        /// New description; an empty value clears it.
        #[arg(long)]
        description: String,
        /// Commit message.
        #[arg(short = 'm', long)]
        message: String,
    },
    /// Make a named version a major checkpoint.
    Promote {
        path: PathBuf,
        version_id: String,
        /// Commit message.
        #[arg(short = 'm', long)]
        message: String,
    },
    /// Make a named version a minor checkpoint.
    Demote {
        path: PathBuf,
        version_id: String,
        /// Commit message.
        #[arg(short = 'm', long)]
        message: String,
    },
    /// Permanently remove a named label without deleting FILE history.
    Tombstone {
        path: PathBuf,
        version_id: String,
        /// Commit message.
        #[arg(short = 'm', long)]
        message: String,
    },
    /// Restore a live named version's bytes into the working tree.
    Restore { path: PathBuf, version_id: String },
}

#[tokio::main]
async fn main() -> ExitCode {
    exit_quietly_when_output_is_closed();
    let json_requested = std::env::args_os().any(|argument| argument == "--json");
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            if json_requested && code != 0 {
                eprintln!(
                    "{}",
                    json!({
                        "schema_version": cli_json::SCHEMA_VERSION,
                        "error": {
                            "kind": "USAGE",
                            "message": error.to_string(),
                        }
                    })
                );
            } else {
                let _ = error.print();
            }
            return ExitCode::from(u8::try_from(code).unwrap_or(1));
        }
    };
    let json = cli.json;
    match run(cli).await {
        Ok(code) => code,
        Err((error, integrity_command)) => {
            if json {
                eprintln!("{}", cli_json::error(&error));
            } else {
                eprintln!("synck: {error}");
            }
            if integrity_command && matches!(error, LocalError::Integrity(_)) {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode, (LocalError, bool)> {
    if let Command::Repository { command } = &cli.command {
        run_portable(command, &cli.lock_token)
            .await
            .map_err(|error| (error, matches!(command, PortableCommand::Verify { .. })))?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Command::Item { command } = &cli.command {
        run_portable_item(command, &cli.lock_token)
            .await
            .map_err(|error| (error, false))?;
        return Ok(ExitCode::SUCCESS);
    }
    if matches!(&cli.command, Command::Instructions) {
        if cli.json {
            return Err((
                LocalError::Invalid("--json is not supported for this command".to_string()),
                false,
            ));
        }
        print!("{INSTRUCTIONS}");
        return Ok(ExitCode::SUCCESS);
    }
    let align_change_markers = terminal_presentation_enabled();
    let current = std::env::current_dir().map_err(|source| {
        (
            LocalError::Io {
                action: "read current directory",
                path: PathBuf::from("."),
                source,
            },
            false,
        )
    })?;
    if let Command::MonitorServer { root, repo_id } = &cli.command {
        synck_local::run_monitor_server(root, repo_id).map_err(|error| (error, false))?;
        return Ok(ExitCode::SUCCESS);
    }
    if cli.json && !supports_json(&cli.command) {
        return Err((
            LocalError::Invalid("--json is not supported for this command".to_string()),
            false,
        ));
    }
    if let Command::Init {
        directory,
        author,
        storage_mode,
        fix_names,
    } = &cli.command
    {
        let (repo_id, repairs) = synck_local::portable::init_repository_with_name_repair(
            directory, author, *storage_mode, *fix_names,
        ).await.map_err(|error| (error, false))?;
        print_name_repairs(&repairs);
        println!("initialized Synck repository {repo_id}");
        return Ok(ExitCode::SUCCESS);
    }
    if matches!(
        cli.command,
        Command::Storage {
            command: Some(StorageCommand::Optimize)
        }
    ) {
        if let Some(changed) = synck_local::portable::optimize_complete_repository(&current)
            .await
            .map_err(|error| (error, false))?
        {
            println!(
                "{}",
                if changed {
                    "storage optimized"
                } else {
                    "storage already optimized"
                }
            );
            return Ok(ExitCode::SUCCESS);
        }
    }
    if matches!(
        cli.command,
        Command::Fsck {
            rebuild_state_index: false
        }
    ) {
        if let Some(report) = synck_local::portable::verify_complete_repository(&current)
            .await
            .map_err(|error| (error, true))?
        {
            println!(
                "{}",
                serde_json::to_string(&report).map_err(|e| (LocalError::from(e), true))?
            );
            return Ok(ExitCode::SUCCESS);
        }
    }
    let integrity_command = matches!(cli.command, Command::Fsck { .. });
    let mut repository = Repository::discover_complete(&current)
        .await
        .map_err(|error| (error, integrity_command))?;
    repository.set_agent_lock_tokens(&cli.lock_token).map_err(|e| (e, false))?;
    if uses_worktree(&cli.command)
        && let Ok(executable) = std::env::current_exe()
    {
        let _ = repository.start_monitor(&executable);
    }
    run_checkout(cli, repository, current, align_change_markers).await
}

async fn run_checkout(
    cli: Cli,
    mut repository: Repository,
    current: PathBuf,
    align_change_markers: bool,
) -> Result<ExitCode, (LocalError, bool)> {
    let json = cli.json;
    // Separate poll frames keep handler temporaries off the dispatcher's stack.
    // A single large async match overflows the Windows main stack in debug builds.
    match cli.command {
        Command::Lock { command } => async {
            let value = match command {
                LockCommand::Acquire { paths, owner, ttl } => {
                    let lease = repository.acquire_agent_locks(&current, &paths, &owner, ttl).await?;
                    json!({"token":lease.token,"locks":lease.locks})
                }
                LockCommand::Renew { token, ttl } => {
                    repository.renew_agent_locks(&token, ttl)?;
                    json!({"renewed":true})
                }
                LockCommand::Release { token } => json!({"released":repository.release_agent_locks(&token)?}),
                LockCommand::List => json!({"locks":repository.agent_locks()?}),
                LockCommand::Break { paths } => json!({"released":repository.break_agent_locks(&current, &paths)?}),
            };
            println!("{}", json!({"schema_version":cli_json::SCHEMA_VERSION,"result":value}));
            Ok::<_, LocalError>(ExitCode::SUCCESS)
        }
        .await.map_err(|error| (error, false)),
        Command::Repository { .. } | Command::Item { .. } => unreachable!(),
        Command::MonitorServer { .. } => unreachable!(),
        Command::Instructions => unreachable!(),
        Command::Init { .. } => unreachable!(),
        Command::Info => async {
            let info = repository.info();
            let head = repository
                .log(Some(1))
                .await
                .map_err(|error| (error, false))?
                .into_iter()
                .next();
            if json {
                println!(
                    "{}",
                    json!({
                        "schema_version": cli_json::SCHEMA_VERSION,
                        "root": info.root.display().to_string(),
                        "format_version": info.format_version,
                        "repo_id": info.repo_id,
                        "author": info.author,
                        "head": head.as_ref().map(|commit| json!({
                            "repo_seq": commit.repo_seq,
                            "commit_id": commit.commit_id,
                        })),
                    })
                );
            } else {
                println!("root {}", info.root.display());
                println!("format_version {}", info.format_version);
                println!("repo_id {}", info.repo_id);
                println!("author {}", info.author);
                if let Some(commit) = head {
                    println!("head_repo_seq {}", commit.repo_seq);
                    println!("head_commit_id {}", commit.commit_id);
                } else {
                    println!("head none");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Context => async {
            let info = repository.info();
            let recent_commits = repository
                .log(Some(CONTEXT_COMMIT_LIMIT))
                .await
                .map_err(|error| (error, false))?;
            let mut changes = repository.status().await.map_err(|error| (error, false))?;
            let changes_total = changes.len();
            changes.truncate(CONTEXT_CHANGE_LIMIT);
            if json {
                println!(
                    "{}",
                    json!({
                        "schema_version": cli_json::SCHEMA_VERSION,
                        "repository": {
                            "root": info.root.display().to_string(),
                            "format_version": info.format_version,
                            "repo_id": info.repo_id,
                            "author": info.author,
                            "head": recent_commits.first().map(|commit| json!({
                                "repo_seq": commit.repo_seq,
                                "commit_id": commit.commit_id,
                            })),
                        },
                        "working_tree": {
                            "clean": changes_total == 0,
                            "changes": changes.iter().map(cli_json::change).collect::<Vec<_>>(),
                            "changes_total": changes_total,
                            "changes_shown": changes.len(),
                            "changes_truncated": changes.len() < changes_total,
                        },
                        "recent_commits": recent_commits
                            .iter()
                            .map(cli_json::log_metadata)
                            .collect::<Vec<_>>(),
                    })
                );
            } else {
                print_context(
                    &info,
                    &recent_commits,
                    &changes,
                    changes_total,
                    align_change_markers,
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Config { command } => async {
            match command {
                ConfigCommand::Author => {
                    let author = repository.info().author;
                    if json {
                        println!(
                            "{}",
                            json!({
                                "schema_version": cli_json::SCHEMA_VERSION,
                                "author": author,
                            })
                        );
                    } else {
                        println!("author {author}");
                    }
                }
                ConfigCommand::SetAuthor { author } => {
                    repository
                        .set_author(&author)
                        .map_err(|error| (error, false))?;
                    if json {
                        println!(
                            "{}",
                            json!({
                                "schema_version": cli_json::SCHEMA_VERSION,
                                "author": author,
                            })
                        );
                    } else {
                        println!("author {author}");
                    }
                }
                ConfigCommand::HiddenFiles => {
                    let mode = repository.hidden_files_mode().as_str();
                    if json {
                        println!(
                            "{}",
                            json!({
                                "schema_version": cli_json::SCHEMA_VERSION,
                                "hidden_files": mode,
                            })
                        );
                    } else {
                        println!("hidden-files {mode}");
                    }
                }
                ConfigCommand::SetHiddenFiles { mode } => {
                    let mode = HiddenFilesMode::from(mode);
                    repository
                        .set_hidden_files_mode(mode)
                        .map_err(|error| (error, false))?;
                    if json {
                        println!(
                            "{}",
                            json!({
                                "schema_version": cli_json::SCHEMA_VERSION,
                                "hidden_files": mode.as_str(),
                            })
                        );
                    } else {
                        println!("hidden-files {}", mode.as_str());
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Status => async {
            let (changes, warnings) = repository.status_with_path_warnings().await.map_err(|error| (error, false))?;
            print_path_warnings(&warnings);
            if json {
                println!("{}", status_json(&changes));
            } else {
                print_status(&changes, align_change_markers);
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Monitor { command } => async {
            let status = match command {
                MonitorCommand::Start => {
                    let executable = std::env::current_exe().map_err(|source| {
                        (
                            LocalError::Io {
                                action: "resolve synck executable",
                                path: PathBuf::from("synck"),
                                source,
                            },
                            false,
                        )
                    })?;
                    repository
                        .start_monitor(&executable)
                        .map_err(|error| (error, false))?
                }
                MonitorCommand::Status => repository.monitor_status(),
                MonitorCommand::Stop => {
                    repository.stop_monitor().map_err(|error| (error, false))?
                }
            };
            print_monitor_status(&status, json);
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Commit {
            message,
            paths,
            fix_names,
        } => async {
                let (repairs, paths) = if fix_names {
                    repository.fix_names_and_remap_paths(&current, &paths).await.map_err(|error| (error, false))?
                } else { (Vec::new(), paths) };
                print_name_repairs(&repairs);
                let caller = if fix_names { repository.repository_root() } else { &current };
                let outcome = repository
                    .commit_selected(caller, &paths, message.as_deref())
                    .await
                    .map_err(|error| (error, false))?;
                if json {
                    let commit_id = match &outcome {
                        CommitOutcome::Clean => None,
                        CommitOutcome::Committed(commit_id) => Some(commit_id),
                    };
                    let mut result = json!({
                        "schema_version": cli_json::SCHEMA_VERSION,
                        "commit_id": commit_id,
                    });
                    if fix_names { result["renamed"] = json!(repairs); }
                    println!("{result}");
                } else {
                    match outcome {
                        CommitOutcome::Clean => println!("nothing to commit"),
                        CommitOutcome::Committed(commit_id) => println!("committed {commit_id}"),
                    }
                }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Revert { revision, message } => async {
            let commit_id = repository
                .revert(&revision, &message)
                .await
                .map_err(|error| (error, false))?;
            if json {
                println!(
                    "{}",
                    json!({"schema_version": cli_json::SCHEMA_VERSION, "commit_id": commit_id})
                );
            } else {
                println!("committed {commit_id}");
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Mv {
            source,
            destination,
            message,
        } => async {
            let commit_id = repository
                .move_path(&current, &source, &destination, &message)
                .await
                .map_err(|error| (error, false))?;
            println!("committed {commit_id}");
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Log {
            limit,
            oneline,
            events,
        } => async {
            if events {
                let commits = repository
                    .detailed_event_log(limit)
                    .await
                    .map_err(|error| (error, false))?;
                if json {
                    println!(
                        "{}",
                        json!({
                            "schema_version": cli_json::SCHEMA_VERSION,
                            "commits": commits
                                .iter()
                                .map(cli_json::detailed_event_log_entry)
                                .collect::<Vec<_>>(),
                        })
                    );
                } else {
                    for (index, entry) in commits.iter().enumerate() {
                        if index != 0 {
                            println!();
                        }
                        print_detailed_event_log_entry(entry);
                    }
                }
            } else if oneline && !json {
                let commits = repository
                    .log(limit)
                    .await
                    .map_err(|error| (error, false))?;
                for commit in commits {
                    let created_at = commit
                        .created_at
                        .with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M %:z")
                        .to_string();
                    let sequence = styled(COMMIT_STYLE, &commit.repo_seq.to_string());
                    let mut header = format!("{sequence} | {created_at} | {}", commit.author);
                    if let Some(message) = commit.message.as_deref() {
                        header.push_str(" | ");
                        header.push_str(&one_line_message(message));
                    }
                    anstream::println!("{header}");
                }
            } else {
                let commits = repository
                    .detailed_log(limit)
                    .await
                    .map_err(|error| (error, false))?;
                if json {
                    println!(
                        "{}",
                        json!({
                            "schema_version": cli_json::SCHEMA_VERSION,
                            "commits": commits
                                .iter()
                                .map(cli_json::detailed_log_entry)
                                .collect::<Vec<_>>(),
                        })
                    );
                } else {
                    for (index, entry) in commits.iter().enumerate() {
                        if index != 0 {
                            println!();
                        }
                        print_detailed_log_entry(entry, align_change_markers);
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Show { revision, events } => async {
            if events {
                let commit = repository
                    .show_events(&revision)
                    .await
                    .map_err(|error| (error, false))?;
                if json {
                    println!(
                        "{}",
                        json!({
                            "schema_version": cli_json::SCHEMA_VERSION,
                            "commit": cli_json::event_history_commit(&commit),
                        })
                    );
                } else {
                    print_event_history_commit(&commit);
                }
            } else {
                let commit = repository
                    .show(&revision)
                    .await
                    .map_err(|error| (error, false))?;
                if json {
                    println!(
                        "{}",
                        json!({
                            "schema_version": cli_json::SCHEMA_VERSION,
                            "commit": cli_json::history_commit(&commit),
                        })
                    );
                } else {
                    print_history_commit(&commit, align_change_markers);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::History { path } => async {
            let commits = repository
                .history(&current, &path)
                .await
                .map_err(|error| (error, false))?;
            if json {
                println!(
                    "{}",
                    json!({
                        "schema_version": cli_json::SCHEMA_VERSION,
                        "commits": commits.iter().map(cli_json::history_commit).collect::<Vec<_>>(),
                    })
                );
            } else {
                for commit in commits {
                    print_history_commit(&commit, align_change_markers);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Compare { from, to } => async {
            let comparison = repository
                .compare(&from, &to)
                .await
                .map_err(|error| (error, false))?;
            if json {
                println!(
                    "{}",
                    json!({
                        "schema_version": cli_json::SCHEMA_VERSION,
                        "from": {
                            "repo_seq": comparison.from.repo_seq,
                            "commit_id": comparison.from.commit_id,
                        },
                        "to": {
                            "repo_seq": comparison.to.repo_seq,
                            "commit_id": comparison.to.commit_id,
                        },
                        "changes": comparison
                            .changes
                            .iter()
                            .map(cli_json::change)
                            .collect::<Vec<_>>(),
                    })
                );
            } else {
                print_changes(&comparison.changes, align_change_markers);
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Diff { revisions } => async {
            let entries = match revisions.as_slice() {
                [] => repository.diff_worktree().await,
                [from, to] => repository.diff_revisions(from, to).await,
                [_] => Err(LocalError::Invalid(
                    "synck diff requires either zero or two revisions".to_string(),
                )),
                _ => unreachable!("clap limits diff to two revisions"),
            }
            .map_err(|error| (error, false))?;
            if json {
                println!(
                    "{}",
                    json!({
                        "schema_version": cli_json::SCHEMA_VERSION,
                        "entries": entries.iter().map(cli_json::text_diff_entry).collect::<Vec<_>>(),
                    })
                );
            } else {
                for entry in &entries {
                    print_text_diff(entry);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Deleted => async {
            let items = repository.deleted().await.map_err(|error| (error, false))?;
            if json {
                println!(
                    "{}",
                    json!({
                        "schema_version": cli_json::SCHEMA_VERSION,
                        "items": items.iter().map(cli_json::deleted_entry).collect::<Vec<_>>(),
                    })
                );
            } else {
                for item in items {
                    println!(
                        "{} {} {} {} {} {}",
                        item.item_id,
                        item.item_type,
                        item.repo_seq,
                        item.commit_id,
                        item.deleted_at.to_rfc3339(),
                        item.path
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Recover { item_id, message } => async {
            let commit_id = repository
                .recover_item(&item_id, &message)
                .await
                .map_err(|error| (error, false))?;
            println!("committed {commit_id}");
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Versions { path } => async {
            let versions = repository
                .versions(&current, &path)
                .await
                .map_err(|error| (error, false))?;
            if json {
                println!(
                    "{}",
                    json!({
                        "schema_version": cli_json::SCHEMA_VERSION,
                        "versions": versions.iter().map(cli_json::version_entry).collect::<Vec<_>>(),
                    })
                );
            } else {
                for version in versions {
                    println!(
                        "{} {} {} {} {}",
                        version.item_seq,
                        version.commit_id,
                        version.created_at.to_rfc3339(),
                        version.blob_id,
                        version.size
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Version { command } => async {
            match command {
                VersionCommand::List { path, all } => {
                    let versions = repository
                        .named_versions(&current, &path, all)
                        .await
                        .map_err(|error| (error, false))?;
                    if json {
                        println!(
                            "{}",
                            json!({
                                "schema_version": cli_json::SCHEMA_VERSION,
                                "versions": versions
                                    .iter()
                                    .map(cli_json::named_version_entry)
                                    .collect::<Vec<_>>(),
                            })
                        );
                    } else {
                        for version in versions {
                            println!(
                                "{} {} {} {} {} {} {} {} {} {} {}",
                                version.label.as_deref().unwrap_or("-"),
                                version.version_id,
                                version.lifecycle_state,
                                version.kind,
                                version.label_source,
                                version.source_item_seq,
                                version.version_set_seq,
                                version.created_at.to_rfc3339(),
                                version.blob_id,
                                version.size,
                                version.description.as_deref().unwrap_or("-")
                            );
                        }
                    }
                }
                VersionCommand::Create {
                    path,
                    item_seq,
                    major,
                    description,
                    message,
                } => {
                    let commit_id = repository
                        .create_named_version(
                            &current,
                            &path,
                            item_seq,
                            if major { "MAJOR" } else { "MINOR" },
                            description.as_deref(),
                            &message,
                        )
                        .await
                        .map_err(|error| (error, false))?;
                    println!("committed {commit_id}");
                }
                VersionCommand::SetDescription {
                    path,
                    version_id,
                    description,
                    message,
                } => {
                    let commit_id = repository
                        .set_named_version_description(
                            &current,
                            &path,
                            &version_id,
                            &description,
                            &message,
                        )
                        .await
                        .map_err(|error| (error, false))?;
                    println!("committed {commit_id}");
                }
                VersionCommand::Promote {
                    path,
                    version_id,
                    message,
                } => {
                    let commit_id = repository
                        .set_named_version_kind(&current, &path, &version_id, "MAJOR", &message)
                        .await
                        .map_err(|error| (error, false))?;
                    println!("committed {commit_id}");
                }
                VersionCommand::Demote {
                    path,
                    version_id,
                    message,
                } => {
                    let commit_id = repository
                        .set_named_version_kind(&current, &path, &version_id, "MINOR", &message)
                        .await
                        .map_err(|error| (error, false))?;
                    println!("committed {commit_id}");
                }
                VersionCommand::Tombstone {
                    path,
                    version_id,
                    message,
                } => {
                    let commit_id = repository
                        .tombstone_named_version(&current, &path, &version_id, &message)
                        .await
                        .map_err(|error| (error, false))?;
                    println!("committed {commit_id}");
                }
                VersionCommand::Restore { path, version_id } => {
                    repository
                        .restore_named_version(&current, &path, &version_id)
                        .await
                        .map_err(|error| (error, false))?;
                    println!(
                        "restored {} from named version {version_id}",
                        path.display()
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Restore { path, item_seq } => async {
            repository
                .restore(&current, &path, item_seq)
                .await
                .map_err(|error| (error, false))?;
            println!("restored {}", path.display());
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::RestoreTree => async {
            if repository
                .restore_worktree()
                .await
                .map_err(|error| (error, false))?
            {
                println!("restored working tree");
            } else {
                println!("nothing to restore");
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Export {
            path,
            destination,
            item_seq,
        } => async {
            repository
                .export(&current, &path, &destination, item_seq)
                .await
                .map_err(|error| (error, false))?;
            println!("exported {} to {}", path.display(), destination.display());
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::ExportTree {
            revision,
            destination,
            folder,
        } => async {
            repository
                .export_tree(&current, &revision, folder.as_deref(), &destination)
                .await
                .map_err(|error| (error, false))?;
            println!("exported {revision} to {}", destination.display());
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Fsck {
            rebuild_state_index,
        } => async {
            if rebuild_state_index {
                repository
                    .rebuild_state_index()
                    .await
                    .map_err(|error| (error, true))?;
            }
            if repository.checkout_paths().is_some() {
                let report = repository
                    .verify_complete()
                    .await
                    .map_err(|error| (error, true))?;
                println!(
                    "{}",
                    serde_json::to_string(&report).map_err(|e| (LocalError::from(e), true))?
                );
                return Ok(ExitCode::SUCCESS);
            }
            let report = repository.fsck().await.map_err(|error| (error, true))?;
            for notice in &report.notices {
                eprintln!("fsck: notice: {notice}");
            }
            let finding_count = report.len();
            let findings_omitted = report.findings_omitted;
            let findings = report.findings;
            if json {
                println!(
                    "{}",
                    json!({
                        "schema_version": cli_json::SCHEMA_VERSION,
                        "ok": finding_count == 0,
                        "findings": findings,
                        "findings_omitted": findings_omitted,
                    })
                );
                if finding_count != 0 {
                    return Ok(ExitCode::from(2));
                }
            } else if findings.is_empty() {
                println!("fsck ok");
            } else {
                for finding in &findings {
                    eprintln!("fsck: {finding}");
                }
                if findings_omitted != 0 {
                    eprintln!("fsck: {findings_omitted} additional finding(s) omitted");
                }
                eprintln!("fsck: {finding_count} finding(s)");
                return Ok(ExitCode::from(2));
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Storage { command } => async {
            match command {
                None => {
                    let report = repository
                        .storage_report()
                        .await
                        .map_err(|error| (error, false))?;
                    if json {
                        println!(
                            "{}",
                            json!({
                                "schema_version": cli_json::SCHEMA_VERSION,
                                "referenced_objects": report.referenced_objects,
                                "referenced_bytes": report.referenced_bytes,
                                "unreachable_objects": report.unreachable_objects,
                                "unreachable_bytes": report.unreachable_bytes,
                            })
                        );
                    } else {
                        println!("referenced_objects {}", report.referenced_objects);
                        println!("referenced_bytes {}", report.referenced_bytes);
                        println!("unreachable_objects {}", report.unreachable_objects);
                        println!("unreachable_bytes {}", report.unreachable_bytes);
                    }
                }
                Some(StorageCommand::Optimize) => {
                    if repository
                        .optimize_storage()
                        .await
                        .map_err(|error| (error, false))?
                    {
                        println!("storage optimized");
                    } else {
                        println!("storage already optimized");
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Gc => async {
            let report = repository.gc().await.map_err(|error| (error, false))?;
            println!("removed_objects {}", report.removed_objects);
            println!("removed_bytes {}", report.removed_bytes);
            Ok(ExitCode::SUCCESS)
        }
        .await,
        Command::Archive {
            destination,
            storage_mode,
        } => async {
            repository
                .archive_complete(&destination, storage_mode)
                .await
                .map_err(|error| (error, false))?;
            println!("archived {}", destination.display());
            Ok(ExitCode::SUCCESS)
        }
        .await,
    }
}

fn parse_storage_mode(value: &str) -> Result<synck_local::StorageMode, String> {
    match value {
        "WHOLE_FILE" => Ok(synck_local::StorageMode::WholeFile),
        "CHUNKED" => Ok(synck_local::StorageMode::Chunked),
        _ => Err("storage mode must be WHOLE_FILE or CHUNKED".into()),
    }
}

fn print_name_repairs(repairs: &[synck_local::NameRepair]) {
    for repair in repairs {
        eprintln!("renamed {:?} -> {:?}", repair.from, repair.to);
    }
}

fn portable_item_id(value: &str) -> synck_local::Result<synck_vcs::types::ItemId> {
    synck_vcs::types::ItemId::parse(value).map_err(|error| LocalError::Invalid(error.to_string()))
}

async fn run_portable(command: &PortableCommand, tokens: &[String]) -> synck_local::Result<()> {
    use synck_local::portable::{PortableRepository, import_repository};
    let open = |path: &Path| -> synck_local::Result<PortableRepository> {
        let mut repository = PortableRepository::open(path)?;
        repository.set_agent_lock_tokens(tokens)?;
        Ok(repository)
    };
    // Keep command temporaries in separate poll frames, as in run_checkout().
    match command {
        PortableCommand::Init {
            destination,
            author,
            storage_mode,
            fix_names,
        } => async {
            let (repo_id, repairs) = synck_local::portable::init_repository_with_name_repair(
                destination,
                author,
                *storage_mode,
                *fix_names,
            )
            .await?;
            print_name_repairs(&repairs);
            println!(
                "{}",
                json!({"schema_version":1,"repo_id":repo_id,"path":destination,"renamed":repairs})
            );
            Ok(())
        }
        .await,
        PortableCommand::Checkout {
            path,
            author,
            paths,
        } => async {
            let mut repository = open(path)?;
            repository.enable_checkout(author, paths).await?;
            print_path_warnings(&repository.checkout_path_warnings().await?);
            println!(
                "{}",
                json!({"schema_version":1,"repo_id":repository.repo_id(),"checkout_paths":repository.checkout_paths(),"authority":repository.authority()})
            );
            Ok(())
        }
        .await,
        PortableCommand::CheckoutSelection { path, paths, .. } => async {
            let mut repository = Repository::discover(path).await?;
            repository.set_agent_lock_tokens(tokens)?;
            repository.set_checkout_paths(paths).await?;
            print_path_warnings(&repository.checkout_path_warnings().await?);
            println!(
                "{}",
                json!({"schema_version":1,"repo_id":repository.repository_id(),"checkout_paths":repository.checkout_paths()})
            );
            Ok(())
        }
        .await,
        PortableCommand::Import {
            source,
            destination,
            storage_mode,
        } => async {
            if let Some(mode) = storage_mode {
                synck_local::portable::import_repository_with_mode(source, destination, *mode)
                    .await?;
            } else {
                import_repository(source, destination).await?;
            }
            println!("{}", json!({"schema_version":1,"imported":destination}));
            Ok(())
        }
        .await,
        PortableCommand::Verify { path } => async {
            let report = match synck_local::portable::verify_complete_repository(path).await? {
                Some(report) => report,
                None => open(path)?.verify().await?,
            };
            println!("{}", serde_json::to_string(&report)?);
            Ok(())
        }
        .await,
        PortableCommand::Log {
            path,
            limit,
            before,
        } => async {
            println!(
                "{}",
                serde_json::to_string(&open(path)?.log(*before, *limit).await?)?
            );
            Ok(())
        }
        .await,
        PortableCommand::Activate {
            path,
            author,
            source_retired,
        } => async {
            let mut repository = open(path)?;
            repository.activate(author, *source_retired).await?;
            println!(
                "{}",
                json!({"schema_version":1,"repo_id":repository.repo_id(),"authority":repository.authority()})
            );
            Ok(())
        }
        .await,
        PortableCommand::Archive {
            path,
            destination,
            storage_mode,
        } => async {
            let repository = open(path)?;
            repository
                .archive_with_mode(
                    destination,
                    storage_mode.unwrap_or(repository.storage_mode()),
                )
                .await?;
            println!("{}", json!({"schema_version":1,"archive":destination}));
            Ok(())
        }
        .await,
        PortableCommand::Finalize { path } => async {
            open(path)?.finalize().await?;
            println!("{}", json!({"schema_version":1,"finalized":path}));
            Ok(())
        }
        .await,
        PortableCommand::CreateFile {
            path,
            parent,
            name,
            source,
            write,
        } => async {
            let options = write.options()?;
            let outcome = open(path)?
                .create_file_with(
                    parent.as_deref().map(portable_item_id).transpose()?,
                    name,
                    source,
                    &options,
                )
                .await?;
            println!(
                "{}",
                json!({"schema_version":1,"item_id":outcome.0,"commit_id":outcome.1})
            );
            Ok(())
        }
        .await,
        PortableCommand::SaveFile {
            path,
            item_id,
            source,
            base_content,
            write,
        } => async {
            let options = write.options()?;
            let base_content = base_content
                .as_deref()
                .map(|value| {
                    synck_vcs::types::ObjectId::parse(value).map_err(|error| {
                        LocalError::Invalid(format!("--base-content must be a blob ID: {error}"))
                    })
                })
                .transpose()?;
            let commit = open(path)?
                .save_file_with(
                    &portable_item_id(item_id)?,
                    source,
                    base_content.as_ref(),
                    &options,
                )
                .await?;
            println!("{}", json!({"schema_version":1,"commit_id":commit}));
            Ok(())
        }
        .await,
        PortableCommand::CreateChat { path, title } => async {
            let (id, commit_id) = open(path)?.create_chat(title).await?;
            println!(
                "{}",
                json!({"schema_version":1,"item_id":id,"commit_id":commit_id})
            );
            Ok(())
        }
        .await,
        PortableCommand::AppendChat {
            path,
            item_id,
            body_file,
            attachments,
        } => async {
            let body = std::fs::read_to_string(body_file).map_err(|source| LocalError::Io {
                action: "read chat body",
                path: body_file.clone(),
                source,
            })?;
            let commit = open(path)?
                .append_chat_message_with_attachments(
                    &portable_item_id(item_id)?,
                    &body,
                    attachments,
                )
                .await?;
            println!("{}", json!({"schema_version":1,"commit_id":commit}));
            Ok(())
        }
        .await,
    }
}

async fn run_portable_item(command: &ItemCommand, tokens: &[String]) -> synck_local::Result<()> {
    use synck_local::portable::{NamespaceCursor, PortableRepository};
    // Keep command temporaries in separate poll frames, as in run_checkout().
    match command {
        ItemCommand::Export {
            path,
            item_id,
            destination,
            through,
        } => async {
            let mut repository = PortableRepository::open(path)?;
            repository.set_agent_lock_tokens(tokens)?;
            let bytes = repository
                .export_file(&portable_item_id(item_id)?, destination, *through)
                .await?;
            println!(
                "{}",
                json!({"schema_version":1,"item_id":item_id,"destination":destination,"bytes":bytes})
            );
            Ok(())
        }
        .await,
        ItemCommand::Show {
            path,
            item_id,
            through,
        } => async {
            println!(
                "{}",
                serde_json::to_string(
                    &PortableRepository::open(path)?
                        .show_item(&portable_item_id(item_id)?, *through)
                        .await?
                )?
            );
            Ok(())
        }
        .await,
        ItemCommand::List {
            path,
            parent,
            after,
            through,
            limit,
        } => async {
            let repository = PortableRepository::open(path)?;
            let parent = parent.as_deref().map(portable_item_id).transpose()?;
            let after = after
                .as_deref()
                .map(|cursor| {
                    serde_json::from_str::<NamespaceCursor>(cursor).map_err(|error| {
                        LocalError::Invalid(format!(
                            "--after must be the exact next_cursor JSON from a previous page: {error}"
                        ))
                    })
                })
                .transpose()?;
            let page = match through {
                Some(seq) => {
                    repository
                        .list_historical_items(*seq, parent.as_ref(), after.as_ref(), *limit)
                        .await?
                }
                None => {
                    repository
                        .list_items(parent.as_ref(), after.as_ref(), *limit)
                        .await?
                }
            };
            println!("{}", serde_json::to_string(&page)?);
            Ok(())
        }
        .await,
    }
}

fn supports_json(command: &Command) -> bool {
    matches!(
        command,
        Command::Lock { .. }
            | Command::Info
            | Command::Context
            | Command::Monitor { .. }
            | Command::Config { .. }
            | Command::Status
            | Command::Commit { .. }
            | Command::Revert { .. }
            | Command::Log { .. }
            | Command::Show { .. }
            | Command::History { .. }
            | Command::Compare { .. }
            | Command::Diff { .. }
            | Command::Deleted
            | Command::Versions { .. }
            | Command::Fsck { .. }
            | Command::Storage { command: None }
            | Command::Version {
                command: VersionCommand::List { .. },
            }
    )
}

fn uses_worktree(command: &Command) -> bool {
    matches!(command, Command::Diff { revisions } if revisions.is_empty())
        || matches!(
            command,
            Command::Context
                | Command::Status
                | Command::Commit { .. }
                | Command::Revert { .. }
                | Command::Mv { .. }
        )
        || matches!(
            command,
            Command::Recover { .. }
                | Command::Restore { .. }
                | Command::RestoreTree
                | Command::Version {
                    command: VersionCommand::Restore { .. },
                }
        )
}

fn print_monitor_status(status: &MonitorStatus, json_output: bool) {
    let (name, instance_id, detail) = match status {
        MonitorStatus::Running { instance_id } => ("running", Some(instance_id.to_string()), None),
        MonitorStatus::Stopped => ("stopped", None, None),
        MonitorStatus::Unavailable { detail } => ("unavailable", None, Some(detail.clone())),
    };
    if json_output {
        println!(
            "{}",
            json!({
                "schema_version": cli_json::SCHEMA_VERSION,
                "status": name,
                "instance_id": instance_id,
                "detail": detail,
            })
        );
    } else if let Some(detail) = detail {
        println!("monitor {name}: {detail}");
    } else {
        println!("monitor {name}");
    }
}

fn print_context(
    info: &RepositoryInfo,
    recent_commits: &[LogEntry],
    changes: &[Change],
    changes_total: usize,
    align_change_markers: bool,
) {
    println!("root {}", info.root.display());
    println!("format_version {}", info.format_version);
    println!("repo_id {}", info.repo_id);
    println!("author {}", info.author);
    if let Some(head) = recent_commits.first() {
        println!("head_repo_seq {}", head.repo_seq);
        println!("head_commit_id {}", head.commit_id);
    } else {
        println!("head none");
    }

    println!();
    let working_tree = if changes_total == 0 {
        styled(HEALTHY_STYLE, "clean")
    } else {
        "dirty".to_string()
    };
    anstream::println!("working_tree {working_tree}");
    print_changes(changes, align_change_markers);
    println!("changes_total {changes_total}");
    println!("changes_shown {}", changes.len());
    println!("changes_truncated {}", changes.len() < changes_total);

    println!();
    println!("recent_commits {}", recent_commits.len());
    for commit in recent_commits {
        let created_at = commit
            .created_at
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M %:z");
        print!("{} | {} | {}", commit.repo_seq, created_at, commit.author);
        if let Some(message) = commit.message.as_deref() {
            println!(" | {}", one_line_message(message));
        } else {
            println!();
        }
    }
}

fn print_text_diff(entry: &TextDiffEntry) {
    match &entry.kind {
        TextDiffKind::Patch(patch) => print!("{patch}"),
        TextDiffKind::Binary => println!(
            "Binary files {} and {} differ",
            diff_path(entry.old_path.as_deref()),
            diff_path(entry.new_path.as_deref())
        ),
        TextDiffKind::TooLarge => println!(
            "Text diff skipped for {} and {}: file exceeds {TEXT_DIFF_MAX_BYTES} bytes",
            diff_path(entry.old_path.as_deref()),
            diff_path(entry.new_path.as_deref())
        ),
    }
}

fn diff_path(path: Option<&str>) -> String {
    path.map_or_else(|| "/dev/null".to_string(), |path| path.to_string())
}

fn one_line_message(message: &str) -> String {
    message.replace(['\r', '\n'], " ")
}

fn print_detailed_log_entry(entry: &DetailedLogEntry, align_change_markers: bool) {
    print_log_header(entry, Some(entry.changes.len()), None);
    let marker_width = change_marker_width(&entry.changes, align_change_markers);
    for change in &entry.changes {
        anstream::println!("    {}", format_change(change, marker_width));
    }
    if entry.changes.len() < entry.changes_total {
        println!(
            "    Run `synck show {}` for the complete change list.",
            entry.commit.repo_seq
        );
    }
}

fn print_detailed_event_log_entry(entry: &DetailedEventLogEntry) {
    print_log_header(&entry.entry, None, Some(entry.events.len()));
    print_commit_events(&entry.events);
}

fn print_log_header(
    entry: &DetailedLogEntry,
    changes_shown: Option<usize>,
    event_count: Option<usize>,
) {
    let commit = &entry.commit;
    let created_at = commit
        .created_at
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M %:z");
    let sequence = styled(COMMIT_STYLE, &commit.repo_seq.to_string());
    let mut header = format!(
        "{sequence} | {created_at} | {} | {}",
        commit.author,
        change_summary(entry.changes_total, changes_shown)
    );
    if let Some(event_count) = event_count {
        header.push_str(" | ");
        header.push_str(&counted(event_count, "event", "events"));
    }
    if let Some(message) = commit.message.as_deref() {
        header.push_str(" | ");
        header.push_str(&one_line_message(message));
    }
    anstream::println!("{header}");
}

fn change_summary(total: usize, shown: Option<usize>) -> String {
    let mut summary = counted(total, "change", "changes");
    if let Some(shown) = shown
        && shown < total
    {
        summary.push_str(&format!(" ({shown} shown)"));
    }
    summary
}

fn counted(count: usize, singular: &str, plural: &str) -> String {
    let noun = if count == 1 { singular } else { plural };
    format!("{count} {noun}")
}

fn print_event_history_commit(value: &EventHistoryCommit) {
    let commit = &value.commit;
    let sequence = styled(COMMIT_STYLE, &commit.repo_seq.to_string());
    let mut header = format!(
        "{sequence} | {} | {} | {} | {}",
        commit.created_at.to_rfc3339(),
        commit.author,
        counted(commit.changes.len(), "change", "changes"),
        counted(value.events.len(), "event", "events")
    );
    if let Some(message) = commit.message.as_deref() {
        header.push_str(" | ");
        header.push_str(&one_line_message(message));
    }
    anstream::println!("{header}");
    println!("    commit {}", commit.commit_id);
    print_commit_events(&value.events);
}

fn print_commit_events(events: &[CommitEvent]) {
    for event in events {
        println!(
            "    {} | {} | item {} | item-seq {}",
            event.commit_event_idx, event.event_type, event.item_id, event.item_seq
        );
        println!("        {}", event.payload);
    }
}

fn print_history_commit(commit: &HistoryCommit, align_change_markers: bool) {
    println!(
        "{} {} {} {} {}",
        commit.repo_seq,
        commit.commit_id,
        commit.created_at.to_rfc3339(),
        commit.author,
        commit.message.as_deref().unwrap_or("")
    );
    print_changes(&commit.changes, align_change_markers);
}

fn print_changes(changes: &[Change], align_change_markers: bool) {
    let marker_width = change_marker_width(changes, align_change_markers);
    for change in changes {
        anstream::println!("{}", format_change(change, marker_width));
    }
}

fn print_status(changes: &[Change], align_change_markers: bool) {
    if changes.is_empty() {
        anstream::println!("{}", styled(HEALTHY_STYLE, CLEAN_STATUS_MESSAGE));
    } else {
        print_changes(changes, align_change_markers);
    }
}

fn change_marker_width(changes: &[Change], align_change_markers: bool) -> Option<usize> {
    if !align_change_markers {
        return None;
    }
    changes.iter().map(|change| change.marker().len()).max()
}

fn format_change(change: &Change, marker_width: Option<usize>) -> String {
    let marker = format_marker(change);
    let separator_width = marker_width.map_or(1, |width| width - change.marker().len() + 1);
    let separator = " ".repeat(separator_width);
    if change.kind.is_relocation() {
        format!(
            "{}{}{} -> {}",
            marker,
            separator,
            change
                .source_path
                .as_deref()
                .expect("relocation has a source path"),
            change.path
        )
    } else {
        format!("{marker}{separator}{}", change.path)
    }
}

fn format_marker(change: &synck_local::Change) -> String {
    let marker = change.marker();
    let style = match change.kind {
        ChangeKind::Add => ADDED_STYLE,
        ChangeKind::Modify => MODIFIED_STYLE,
        ChangeKind::Delete => DELETED_STYLE,
        ChangeKind::Rename | ChangeKind::Move | ChangeKind::MoveRename
            if change.content_changed =>
        {
            MODIFIED_STYLE
        }
        ChangeKind::Rename | ChangeKind::Move | ChangeKind::MoveRename => HEALTHY_STYLE,
    };
    styled(style, marker)
}

fn styled(style: Style, text: &str) -> String {
    format!("{}{text}{}", style.render(), style.render_reset())
}

fn terminal_presentation_enabled() -> bool {
    let stdout = std::io::stdout();
    stdout.is_terminal() || anstream::AutoStream::choice(&stdout) != anstream::ColorChoice::Never
}

fn status_json(changes: &[synck_local::Change]) -> Value {
    json!({
        "schema_version": cli_json::SCHEMA_VERSION,
        "changes": changes.iter().map(cli_json::change).collect::<Vec<_>>(),
    })
}

fn print_path_warnings(paths: &[PathBuf]) {
    for path in paths {
        eprintln!("warning: path exceeds 260 UTF-16 units; other Windows applications may reject it: {:?}", path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synck_local::{Change, ChangeKind};

    #[test]
    fn status_rendering_uses_kind_for_arrows_and_schema_five_json() {
        let changes = vec![
            Change {
                kind: ChangeKind::Rename,
                item_type: "FOLDER".to_string(),
                path: "Renamed".to_string(),
                source_path: Some("Folder".to_string()),
                content_changed: false,
            },
            Change {
                kind: ChangeKind::Modify,
                item_type: "FILE".to_string(),
                path: "Renamed/file.txt".to_string(),
                source_path: Some("Folder/file.txt".to_string()),
                content_changed: true,
            },
        ];
        let marker_width = change_marker_width(&changes, true);

        assert_eq!(
            format_change(&changes[0], marker_width),
            format!("{} Folder -> Renamed", styled(HEALTHY_STYLE, "RN"))
        );
        assert_eq!(
            format_change(&changes[1], marker_width),
            format!("{}  Renamed/file.txt", styled(MODIFIED_STYLE, "M"))
        );
        assert_eq!(
            format_change(&changes[0], None),
            format!("{} Folder -> Renamed", styled(HEALTHY_STYLE, "RN"))
        );
        assert_eq!(status_json(&changes)["schema_version"], 5);
        assert_eq!(status_json(&changes)["changes"][1]["kind"], "MODIFY");
    }

    #[test]
    fn marker_colors_use_one_dominant_meaning() {
        let change = |kind, content_changed| Change {
            kind,
            item_type: "FILE".to_string(),
            path: "Folder/file.txt".to_string(),
            source_path: kind.is_relocation().then(|| "Source/file.txt".to_string()),
            content_changed,
        };

        assert_eq!(
            format_marker(&change(ChangeKind::Add, false)),
            styled(ADDED_STYLE, "A")
        );
        assert_eq!(
            format_marker(&change(ChangeKind::Modify, true)),
            styled(MODIFIED_STYLE, "M")
        );
        assert_eq!(
            format_marker(&change(ChangeKind::Delete, false)),
            styled(DELETED_STYLE, "D")
        );
        assert_eq!(
            format_marker(&change(ChangeKind::MoveRename, false)),
            styled(HEALTHY_STYLE, "MV+RN")
        );
        assert_eq!(
            format_marker(&change(ChangeKind::Move, true)),
            styled(MODIFIED_STYLE, "M+MV")
        );
    }
}
