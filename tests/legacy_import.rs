use std::path::Path;

use hoglet::control::ProjectAccess;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

pub mod control {
    pub use hoglet::control::*;
}

pub mod storage_bootstrap {
    pub use hoglet::storage_bootstrap::*;
}

pub mod token {
    pub use hoglet::token::*;
}

#[path = "../src/legacy_import.rs"]
mod legacy_import;

fn create_legacy_auth(path: &Path) {
    let key_secret = "phx_0123456789abcdef0123456789abcdef";
    let key_hash = hex::encode(Sha256::digest(key_secret.as_bytes()));
    let connection = Connection::open(path).expect("legacy auth database should open");
    connection
        .execute_batch(
            "CREATE TABLE users (
                 id TEXT PRIMARY KEY, email TEXT UNIQUE, pw_hash TEXT, name TEXT, created_at INTEGER
             );
             CREATE TABLE orgs (id TEXT PRIMARY KEY, name TEXT, created_at INTEGER);
             CREATE TABLE org_members (org_id TEXT, user_id TEXT, role TEXT);
             CREATE TABLE projects (
                 id TEXT PRIMARY KEY, org_id TEXT, name TEXT, token TEXT UNIQUE, created_at INTEGER
             );
             CREATE TABLE personal_api_keys (
                 id TEXT PRIMARY KEY, user_id TEXT, name TEXT, key_hash TEXT,
                 key_prefix TEXT, last_used INTEGER, created_at INTEGER
             );
             CREATE TABLE sessions (
                 id TEXT PRIMARY KEY, user_id TEXT, created_at INTEGER, expires_at INTEGER
             );",
        )
        .expect("legacy auth schema should be created");
    connection
        .execute(
            "INSERT INTO users VALUES ('user-1','owner@example.com','legacy-hash','Owner',10)",
            [],
        )
        .expect("legacy user should be inserted");
    connection
        .execute(
            "INSERT INTO orgs VALUES ('org-1','Original Organization',11)",
            [],
        )
        .expect("legacy organization should be inserted");
    connection
        .execute(
            "INSERT INTO org_members VALUES ('org-1','user-1','owner')",
            [],
        )
        .expect("legacy membership should be inserted");
    connection
        .execute(
            "INSERT INTO projects VALUES ('project-1','org-1','Auth Project','phc_auth',12)",
            [],
        )
        .expect("legacy project should be inserted");
    connection
        .execute(
            "INSERT INTO personal_api_keys VALUES (
                 'key-1','user-1','CLI',?1,'phx_0123456',NULL,13
             )",
            [key_hash],
        )
        .expect("legacy personal key should be inserted");
    connection
        .execute(
            "INSERT INTO sessions VALUES ('session-1','user-1',14,4102444800)",
            [],
        )
        .expect("legacy session should be inserted");
}

fn create_legacy_token_sources(directory: &Path) {
    let identity = Connection::open(directory.join("identity.db"))
        .expect("legacy identity database should open");
    identity
        .execute_batch(
            "CREATE TABLE persons (id INTEGER, token TEXT);
             INSERT INTO persons VALUES (1,'phc_identity');",
        )
        .expect("legacy identity token should be inserted");

    let registry = Connection::open(directory.join("projects.db"))
        .expect("legacy registry database should open");
    registry
        .execute_batch(
            "CREATE TABLE projects (token TEXT PRIMARY KEY, name TEXT, created_at TEXT);
             INSERT INTO projects VALUES ('phc_auth','Registry Must Lose','2026-01-01T00:00:00Z');
             INSERT INTO projects VALUES ('phc_identity','Identity Project','2026-01-01T00:00:00Z');",
        )
        .expect("legacy registry projects should be inserted");
}

#[tokio::test]
async fn auth_projects_win_while_orphan_tokens_become_accessible_imported_projects() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let control_path = directory.path().join("control.db");
    let projections_path = directory.path().join("projections.db");
    storage_bootstrap::bootstrap_storage(&control_path, &projections_path)
        .expect("storage pair should bootstrap");
    create_legacy_auth(&directory.path().join("auth.db"));
    create_legacy_token_sources(directory.path());

    let report = legacy_import::import_legacy_control(directory.path(), &control_path)
        .expect("legacy control state should import");
    assert_eq!(report.users, 1);
    assert_eq!(report.projects, 2);

    let (access, runtime) =
        ProjectAccess::open(control_path.clone()).expect("imported project access should open");
    let session_principal = access
        .validate_session("session-1")
        .await
        .expect("legacy session should remain usable");
    let workspace = access
        .workspace(&session_principal)
        .await
        .expect("legacy owner should have a workspace");
    let projects = workspace
        .organizations
        .iter()
        .flat_map(|organization| organization.projects.iter())
        .collect::<Vec<_>>();
    assert_eq!(projects.len(), 2);
    assert!(
        projects
            .iter()
            .any(|project| project.token == "phc_auth" && project.name == "Auth Project")
    );
    assert!(
        projects
            .iter()
            .any(|project| project.token == "phc_identity" && project.name == "Identity Project")
    );
    let key_principal = access
        .validate_personal_key("phx_0123456789abcdef0123456789abcdef")
        .await
        .expect("legacy personal key should remain usable");
    assert_eq!(key_principal.user_id, "user-1");

    drop(access);
    runtime.close();

    let second = legacy_import::import_legacy_control(directory.path(), &control_path)
        .expect("rerunning the legacy import should be safe");
    assert_eq!(second.users, 1);
    assert_eq!(second.organizations, 2);
    assert_eq!(second.projects, 2);
    assert_eq!(second.personal_keys, 1);
    assert_eq!(second.sessions, 1);
}

#[test]
fn an_invalid_legacy_token_leaves_control_state_unmodified() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let control_path = directory.path().join("control.db");
    let projections_path = directory.path().join("projections.db");
    storage_bootstrap::bootstrap_storage(&control_path, &projections_path)
        .expect("storage pair should bootstrap");
    let auth_path = directory.path().join("auth.db");
    create_legacy_auth(&auth_path);
    Connection::open(auth_path)
        .expect("legacy auth database should reopen")
        .execute(
            "INSERT INTO projects VALUES (
                 'project-invalid','org-1','Invalid','phx_not-a-project',15
             )",
            [],
        )
        .expect("invalid legacy project should be seeded");

    let error = legacy_import::import_legacy_control(directory.path(), &control_path)
        .expect_err("a personal API key cannot be imported as a project token");
    assert!(matches!(
        error,
        legacy_import::LegacyImportError::InvalidToken(token)
            if token == "phx_not-a-project"
    ));

    let control = Connection::open(control_path).expect("control database should open");
    let user_count: i64 = control
        .query_row("SELECT count(*) FROM users", [], |row| row.get(0))
        .expect("control user count should be readable");
    let project_count: i64 = control
        .query_row("SELECT count(*) FROM projects", [], |row| row.get(0))
        .expect("control project count should be readable");
    assert_eq!(user_count, 0);
    assert_eq!(project_count, 0);
}
