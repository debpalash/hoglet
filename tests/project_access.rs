use hoglet::control::{
    AccessError, LoginRequest, ProjectAccess, ProjectAccessRuntime, SetupRequest,
};
use hoglet::storage_bootstrap::bootstrap_storage;

fn open_access(directory: &std::path::Path) -> (ProjectAccess, ProjectAccessRuntime) {
    let control = directory.join("control.db");
    bootstrap_storage(&control, directory.join("projections.db")).unwrap();
    ProjectAccess::open(control).unwrap()
}

#[test]
fn project_access_refuses_an_unvalidated_or_wrong_role_database() {
    let dir = tempfile::tempdir().unwrap();
    let arbitrary = dir.path().join("arbitrary.db");
    rusqlite::Connection::open(&arbitrary).unwrap();

    assert!(matches!(
        ProjectAccess::open(arbitrary),
        Err(AccessError::InvalidStorage)
    ));

    let control = dir.path().join("control.db");
    let projections = dir.path().join("projections.db");
    bootstrap_storage(&control, &projections).unwrap();
    assert!(matches!(
        ProjectAccess::open(projections),
        Err(AccessError::InvalidStorage)
    ));
}

#[tokio::test]
async fn runtime_shutdown_does_not_wait_for_project_access_clones() {
    let dir = tempfile::tempdir().unwrap();
    let (access, runtime) = open_access(dir.path());
    let retained_clone = access.clone();

    runtime.close();

    assert!(matches!(
        retained_clone.setup_required().await,
        Err(AccessError::Unavailable)
    ));
}

#[tokio::test]
async fn setup_creates_the_requested_capture_token_without_open_mode() {
    let dir = tempfile::tempdir().unwrap();
    let (access, runtime) = open_access(dir.path());

    assert!(matches!(
        access.authorize_capture("phc_existing").await,
        Err(AccessError::Unauthorized)
    ));

    let setup = access
        .setup(SetupRequest {
            email: "owner@example.com".into(),
            password: "correct horse battery staple".into(),
            organization_name: "Acme".into(),
            project_name: "Product".into(),
            existing_project_token: Some("phc_existing".into()),
        })
        .await
        .unwrap();

    assert_eq!(setup.workspace.organizations.len(), 1);
    let project = &setup.workspace.organizations[0].projects[0];
    assert_eq!(project.token, "phc_existing");
    let capture = access.authorize_capture("phc_existing").await.unwrap();
    assert_eq!(capture.project_id.as_deref(), Some(project.id.as_str()));

    let principal = access.validate_session(&setup.session_id).await.unwrap();
    let authorized = access
        .authorize_project(&principal, &project.id)
        .await
        .unwrap();
    assert_eq!(authorized.capture_token, "phc_existing");
    assert_eq!(authorized.role.as_str(), "owner");

    assert!(access.authorize_capture("phc_unknown").await.is_err());

    drop(access);
    runtime.close();
}

#[tokio::test]
async fn setup_is_single_use_and_leaves_the_first_workspace_intact() {
    let dir = tempfile::tempdir().unwrap();
    let (access, runtime) = open_access(dir.path());
    let request = SetupRequest {
        email: "owner@example.com".into(),
        password: "correct horse battery staple".into(),
        organization_name: "Acme".into(),
        project_name: "Product".into(),
        existing_project_token: None,
    };

    let first = access.setup(request.clone()).await.unwrap();
    assert!(access.setup(request).await.is_err());

    let principal = access.validate_session(&first.session_id).await.unwrap();
    let workspace = access.workspace(&principal).await.unwrap();
    assert_eq!(workspace.user.email, "owner@example.com");
    assert_eq!(workspace.organizations.len(), 1);
    assert_eq!(workspace.organizations[0].projects.len(), 1);

    drop(access);
    runtime.close();
}

#[tokio::test]
async fn bootstrap_login_and_logout_share_one_authoritative_session_store() {
    let dir = tempfile::tempdir().unwrap();
    let (access, runtime) = open_access(dir.path());

    assert!(access.setup_required().await.unwrap());
    let setup = access
        .setup(SetupRequest {
            email: "owner@example.com".into(),
            password: "correct horse battery staple".into(),
            organization_name: "Acme".into(),
            project_name: "Product".into(),
            existing_project_token: None,
        })
        .await
        .unwrap();
    assert!(!access.setup_required().await.unwrap());

    let wrong = access
        .login(LoginRequest {
            email: "owner@example.com".into(),
            password: "wrong".into(),
        })
        .await;
    assert!(matches!(wrong, Err(AccessError::InvalidCredentials)));

    let login = access
        .login(LoginRequest {
            email: "OWNER@example.com".into(),
            password: "correct horse battery staple".into(),
        })
        .await
        .unwrap();
    assert_eq!(login.workspace.user.id, setup.workspace.user.id);

    access.logout(&login.session_id).await.unwrap();
    assert!(matches!(
        access.validate_session(&login.session_id).await,
        Err(AccessError::Unauthorized)
    ));

    drop(access);
    runtime.close();
}

#[tokio::test]
async fn personal_keys_authenticate_the_same_principal_and_are_user_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let (access, runtime) = open_access(dir.path());
    let setup = access
        .setup(SetupRequest {
            email: "owner@example.com".into(),
            password: "correct horse battery staple".into(),
            organization_name: "Acme".into(),
            project_name: "Product".into(),
            existing_project_token: None,
        })
        .await
        .unwrap();
    let owner = access.validate_session(&setup.session_id).await.unwrap();

    let created = access
        .create_personal_key(&owner, "automation")
        .await
        .unwrap();
    assert!(created.secret.starts_with("phx_"));
    let from_key = access.validate_personal_key(&created.secret).await.unwrap();
    assert_eq!(from_key.user_id, owner.user_id);

    let keys = access.list_personal_keys(&owner).await.unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].id, created.key.id);
    assert!(keys[0].last_used.is_some());

    access
        .revoke_personal_key(&owner, &created.key.id)
        .await
        .unwrap();
    assert!(matches!(
        access.validate_personal_key(&created.secret).await,
        Err(AccessError::Unauthorized)
    ));

    drop(access);
    runtime.close();
}

#[tokio::test]
async fn organization_creation_and_project_creation_enforce_membership() {
    let dir = tempfile::tempdir().unwrap();
    let (access, runtime) = open_access(dir.path());
    let setup = access
        .setup(SetupRequest {
            email: "owner@example.com".into(),
            password: "correct horse battery staple".into(),
            organization_name: "Acme".into(),
            project_name: "Product".into(),
            existing_project_token: None,
        })
        .await
        .unwrap();
    let owner = access.validate_session(&setup.session_id).await.unwrap();

    let organization = access
        .create_organization(&owner, "Second Org")
        .await
        .unwrap();
    let project = access
        .create_project(&owner, &organization.id, "Second Product")
        .await
        .unwrap();
    assert!(project.token.starts_with("phc_"));

    let authorized = access.authorize_project(&owner, &project.id).await.unwrap();
    assert_eq!(authorized.organization_id, organization.id);
    assert_eq!(authorized.role.as_str(), "owner");

    let workspace = access.workspace(&owner).await.unwrap();
    assert_eq!(workspace.organizations.len(), 2);
    assert_eq!(workspace.organizations[1].projects[0].id, project.id);

    drop(access);
    runtime.close();
}

#[tokio::test]
async fn setup_claims_a_preimported_project_without_duplicating_its_token() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("control.db");
    let (access, runtime) = open_access(dir.path());
    let imported_org_id = "00000000-0000-0000-0000-000000000001";
    let imported_project_id = "00000000-0000-0000-0000-000000000002";
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .execute(
            "INSERT INTO organizations(id,name,created_at,imported) VALUES (?1,'Imported Projects',0,1)",
            [imported_org_id],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO projects(id,organization_id,name,capture_token,created_at,imported)
             VALUES (?1,?2,'Imported Project','phc_imported',0,1)",
            [imported_project_id, imported_org_id],
        )
        .unwrap();
    drop(connection);

    let setup = access
        .setup(SetupRequest {
            email: "owner@example.com".into(),
            password: "correct horse battery staple".into(),
            organization_name: "Ignored for claimed imports".into(),
            project_name: "Ignored for claimed imports".into(),
            existing_project_token: Some("phc_imported".into()),
        })
        .await
        .unwrap();

    assert_eq!(setup.workspace.organizations.len(), 1);
    assert_eq!(setup.workspace.organizations[0].id, imported_org_id);
    assert_eq!(setup.workspace.organizations[0].projects.len(), 1);
    assert_eq!(
        setup.workspace.organizations[0].projects[0].id,
        imported_project_id
    );

    drop(access);
    runtime.close();
}
