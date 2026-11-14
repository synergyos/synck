//! `synck serve`: the reference sync server's command line.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use synck_local::portable::ServedRepository;
use synck_local::server::tokens::{self, Access};
use synck_local::server::{Error, Result, Server};

/// `synck serve STATE --listen ADDRESS REPOSITORY...` serves; `init` and
/// `token` prepare and administer its state root.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub struct ServeArgs {
    #[command(subcommand)]
    command: Option<ServeCommand>,
    /// State root made by `synck serve init`.
    #[arg(required = true)]
    state: Option<PathBuf>,
    /// Loopback address; a TLS proxy on the same host faces the network.
    #[arg(long, required = true)]
    listen: Option<SocketAddr>,
    /// Repository directories that order their own history.
    #[arg(required = true)]
    repositories: Vec<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum ServeCommand {
    /// Create a private state root: its lock and token database.
    Init {
        /// Directory for the state root, made if absent.
        state: PathBuf,
    },
    /// Manage named bearer tokens. They take effect while the server runs.
    #[command(subcommand)]
    Token(TokenCommand),
}

#[derive(Debug, Subcommand)]
enum TokenCommand {
    /// Print a new token once; only its hash is kept.
    Add {
        /// State root made by `synck serve init`.
        state: PathBuf,
        /// Unique name to list and revoke the token by.
        #[arg(long)]
        name: String,
        /// `read` clones and pulls; `write` also commits and leases.
        #[arg(long, value_enum)]
        access: Access,
        /// A repository directory the token may use. Repeat for more.
        #[arg(long = "repository", required = true)]
        repositories: Vec<PathBuf>,
    },
    /// Print each token's name, access, creation time and repository IDs.
    List {
        /// State root made by `synck serve init`.
        state: PathBuf,
    },
    /// Refuse a token from its next request on.
    Revoke {
        /// State root made by `synck serve init`.
        state: PathBuf,
        /// The token's name.
        name: String,
    },
}

fn state_root(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|error| Error::filesystem("resolve state root", path, error))
}

pub async fn run(args: ServeArgs) -> Result<()> {
    match args.command {
        Some(ServeCommand::Init { state }) => synck_local::server::init(&state_root(&state)?),
        Some(ServeCommand::Token(TokenCommand::Add {
            state,
            name,
            access,
            repositories,
        })) => {
            let repositories = repositories
                .iter()
                .map(|root| Ok(ServedRepository::open(root)?.repository_id().clone()))
                .collect::<Result<Vec<_>>>()?;
            let text = tokens::add(&state_root(&state)?, &name, access, &repositories)?;
            eprintln!("token {name} ({access}); it is not shown again");
            println!("{text}");
            Ok(())
        }
        Some(ServeCommand::Token(TokenCommand::List { state })) => {
            for token in tokens::list(&state_root(&state)?)? {
                let created = token
                    .created_at
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs());
                let repositories: Vec<&str> = token
                    .repositories
                    .iter()
                    .map(|repo| repo.as_str())
                    .collect();
                println!(
                    "{}\t{}\t{created}\t{}",
                    token.name,
                    token.access,
                    repositories.join(",")
                );
            }
            Ok(())
        }
        Some(ServeCommand::Token(TokenCommand::Revoke { state, name })) => {
            if !tokens::revoke(&state_root(&state)?, &name)? {
                return Err(Error::Invalid(format!("no token is named {name}")));
            }
            Ok(())
        }
        None => {
            let (Some(state), Some(listen)) = (args.state, args.listen) else {
                unreachable!("clap requires the state root and address without a subcommand");
            };
            let server = Server::open(&state_root(&state)?, &args.repositories)?;
            for repository in server.repositories() {
                eprintln!(
                    "serving {} from {}",
                    repository.repository_id(),
                    repository.root().display()
                );
            }
            synck_local::server::serve(server, listen).await
        }
    }
}
