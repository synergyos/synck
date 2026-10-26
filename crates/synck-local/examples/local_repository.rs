//! Run with an absent destination directory; the created repository is retained.

use std::fs;
use std::io;
use std::path::PathBuf;

use synck_local::{CommitOutcome, Repository, init};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root =
        PathBuf::from(std::env::args_os().nth(1).ok_or_else(|| {
            io::Error::other("usage: local_repository <new-repository-directory>")
        })?);
    fs::create_dir(&root)?;
    init(&root, "example").await?;
    fs::write(root.join("hello.txt"), b"hello\n")?;
    let repository = Repository::discover(&root).await?;
    let CommitOutcome::Committed(first_commit) = repository.commit("Add hello").await? else {
        return Err(io::Error::other("expected a new file commit").into());
    };

    fs::write(root.join("hello.txt"), b"hello again\n")?;
    let CommitOutcome::Committed(second_commit) = repository.commit("Update hello").await? else {
        return Err(io::Error::other("expected an updated file commit").into());
    };
    assert_eq!(fs::read(root.join("hello.txt"))?, b"hello again\n");

    let view = repository.open_historical_view(&first_commit).await?;
    let children = repository.historical_children(&view, None, None).await?;
    let file = children
        .entries
        .iter()
        .find(|item| item.name == "hello.txt")
        .ok_or_else(|| io::Error::other("historical file is missing"))?;
    let cache = tempfile::tempdir()?;
    let historical = repository
        .materialize_historical_file(&view, &file.item_id, cache.path())
        .await?;
    assert_eq!(fs::read(&historical.path)?, b"hello\n");
    let commits = repository.log(Some(10)).await?;
    assert_eq!(commits[0].commit_id, second_commit);
    assert_eq!(commits[1].commit_id, first_commit);
    let report = repository.fsck().await?;
    assert!(report.is_empty(), "integrity findings: {report:?}");
    println!(
        "Created and verified two file versions at {}",
        root.display()
    );
    Ok(())
}
