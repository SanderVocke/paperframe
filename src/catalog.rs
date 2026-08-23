use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use sqlparser::{ast::Statement, dialect::SQLiteDialect, parser::Parser};

use crate::config::DigikamSettings;

const REQUIRED_COLUMNS: [&str; 4] = ["image_id", "root_path", "album_path", "file_name"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub image_id: i64,
    pub root_path: PathBuf,
    pub album_path: PathBuf,
    pub file_name: PathBuf,
}

pub async fn validate(settings: DigikamSettings) -> Result<()> {
    tokio::task::spawn_blocking(move || validate_blocking(&settings))
        .await
        .context("catalog validation worker stopped")?
}

pub async fn candidates(settings: DigikamSettings) -> Result<Vec<Candidate>> {
    tokio::task::spawn_blocking(move || candidates_blocking(&settings))
        .await
        .context("catalog query worker stopped")?
}

fn validate_blocking(settings: &DigikamSettings) -> Result<()> {
    validate_statement_shape(&settings.query)?;
    let connection = open_read_only(settings)?;
    let statement = connection
        .prepare(&settings.query)
        .context("failed to prepare DigiKam query")?;
    validate_prepared_statement(&statement)
}

fn candidates_blocking(settings: &DigikamSettings) -> Result<Vec<Candidate>> {
    validate_statement_shape(&settings.query)?;
    let connection = open_read_only(settings)?;
    let mut statement = connection
        .prepare(&settings.query)
        .context("failed to prepare DigiKam query")?;
    validate_prepared_statement(&statement)?;

    let mut rows = statement
        .query([])
        .context("failed to execute DigiKam query")?;
    let mut candidates = Vec::new();
    while let Some(row) = rows
        .next()
        .context("failed while reading DigiKam query results")?
    {
        if candidates.len() == settings.max_candidates {
            bail!(
                "DigiKam query returned more than max_candidates ({})",
                settings.max_candidates
            );
        }
        candidates.push(Candidate {
            image_id: row.get("image_id").context("image_id must be an integer")?,
            root_path: PathBuf::from(
                row.get::<_, String>("root_path")
                    .context("root_path must be text")?,
            ),
            album_path: PathBuf::from(
                row.get::<_, String>("album_path")
                    .context("album_path must be text")?,
            ),
            file_name: PathBuf::from(
                row.get::<_, String>("file_name")
                    .context("file_name must be text")?,
            ),
        });
    }
    Ok(candidates)
}

fn open_read_only(settings: &DigikamSettings) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(&settings.database_path, flags)
        .context("failed to open DigiKam database read-only")?;
    connection
        .pragma_update(None, "query_only", true)
        .context("failed to enable SQLite query-only mode")?;
    Ok(connection)
}

fn validate_statement_shape(query: &str) -> Result<()> {
    let statements = Parser::parse_sql(&SQLiteDialect {}, query)
        .context("query is not valid SQLite SELECT syntax")?;
    if statements.len() != 1 {
        bail!("query must contain exactly one statement");
    }
    if !matches!(statements.first(), Some(Statement::Query(_))) {
        bail!("query must be a SELECT or CTE");
    }
    Ok(())
}

fn validate_prepared_statement(statement: &rusqlite::Statement<'_>) -> Result<()> {
    if !statement.readonly() {
        bail!("SQLite reports that the query can modify the database");
    }
    let columns = statement.column_names();
    for required in REQUIRED_COLUMNS {
        if !columns.contains(&required) {
            bail!("query must return a column aliased as {required}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::params;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use crate::config::PathMapping;

    use super::*;

    fn create_catalog(path: &std::path::Path, image_count: usize) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE AlbumRoots (id INTEGER PRIMARY KEY, specificPath TEXT NOT NULL);
                 CREATE TABLE Albums (id INTEGER PRIMARY KEY, albumRoot INTEGER NOT NULL, relativePath TEXT NOT NULL);
                 CREATE TABLE Images (id INTEGER PRIMARY KEY, album INTEGER NOT NULL, name TEXT NOT NULL);
                 INSERT INTO AlbumRoots VALUES (1, '/library');
                 INSERT INTO Albums VALUES (2, 1, '/holidays');",
            )
            .unwrap();
        for id in 0..image_count {
            connection
                .execute(
                    "INSERT INTO Images VALUES (?1, 2, ?2)",
                    params![id as i64 + 10, format!("photo-{id}.jpg")],
                )
                .unwrap();
        }
    }

    fn settings(database_path: PathBuf) -> DigikamSettings {
        DigikamSettings {
            database_path,
            query: "SELECT Images.id AS image_id, AlbumRoots.specificPath AS root_path, Albums.relativePath AS album_path, Images.name AS file_name FROM Images JOIN Albums ON Albums.id = Images.album JOIN AlbumRoots ON AlbumRoots.id = Albums.albumRoot ORDER BY Images.id".into(),
            path_mappings: vec![PathMapping {
                database_root: PathBuf::from("/library"),
                mounted_root: PathBuf::from("/photos"),
            }],
            max_candidates: 100,
        }
    }

    fn digest(path: &std::path::Path) -> Vec<u8> {
        Sha256::digest(fs::read(path).unwrap()).to_vec()
    }

    #[tokio::test]
    async fn reads_candidates_without_changing_database_or_creating_sidecars() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("digikam.db");
        create_catalog(&database, 2);
        let before = digest(&database);

        let result = candidates(settings(database.clone())).await.unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].image_id, 10);
        assert_eq!(result[0].root_path, PathBuf::from("/library"));
        assert_eq!(result[0].album_path, PathBuf::from("/holidays"));
        assert_eq!(result[0].file_name, PathBuf::from("photo-0.jpg"));
        assert_eq!(digest(&database), before);
        assert!(!directory.path().join("digikam.db-journal").exists());
        assert!(!directory.path().join("digikam.db-wal").exists());
        assert!(!directory.path().join("digikam.db-shm").exists());
    }

    #[tokio::test]
    async fn accepts_a_read_only_cte() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("digikam.db");
        create_catalog(&database, 1);
        let mut input = settings(database);
        input.query = "WITH chosen AS (SELECT Images.id, Images.album, Images.name FROM Images) SELECT chosen.id AS image_id, AlbumRoots.specificPath AS root_path, Albums.relativePath AS album_path, chosen.name AS file_name FROM chosen JOIN Albums ON Albums.id = chosen.album JOIN AlbumRoots ON AlbumRoots.id = Albums.albumRoot".into();

        validate(input).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_mutation_ddl_pragma_and_multiple_statements() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("digikam.db");
        create_catalog(&database, 1);
        for query in [
            "UPDATE Images SET name = 'bad.jpg'",
            "DELETE FROM Images",
            "CREATE TABLE bad (id INTEGER)",
            "PRAGMA user_version = 2",
            "SELECT 1; SELECT 2",
        ] {
            let mut input = settings(database.clone());
            input.query = query.into();
            assert!(
                validate(input).await.is_err(),
                "query unexpectedly accepted: {query}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_missing_aliases_and_wrong_types() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("digikam.db");
        create_catalog(&database, 1);
        let mut missing = settings(database.clone());
        missing.query = "SELECT 1 AS image_id, '/x' AS root_path, '/' AS album_path".into();
        assert!(
            validate(missing)
                .await
                .unwrap_err()
                .to_string()
                .contains("file_name")
        );

        let mut wrong_type = settings(database);
        wrong_type.query = "SELECT 'not-an-id' AS image_id, '/x' AS root_path, '/' AS album_path, 'x.jpg' AS file_name".into();
        assert!(
            candidates(wrong_type)
                .await
                .unwrap_err()
                .to_string()
                .contains("image_id")
        );
    }

    #[tokio::test]
    async fn enforces_candidate_limit() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("digikam.db");
        create_catalog(&database, 3);
        let mut input = settings(database);
        input.max_candidates = 2;

        assert!(
            candidates(input)
                .await
                .unwrap_err()
                .to_string()
                .contains("max_candidates")
        );
    }

    #[tokio::test]
    async fn reports_unavailable_database_without_creating_it() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("missing.db");

        assert!(validate(settings(database.clone())).await.is_err());
        assert!(!database.exists());
    }
}
