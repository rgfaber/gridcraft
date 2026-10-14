//! GridCraft's hooks onto the mesh: `--collab <config.json>`, invite joins and the realm sign-in,
//! all through macula-craft-collab's [`CraftMesh`] (the config format is documented there). This
//! file only installs what it hands out: the shared session into the GridCraft session, and the
//! person's mesh presence for the Share and Sign in windows' hooks.

use gridcraft_engine::Session;
use macula_craft_collab::CraftMesh;

/// This person's mesh presence. The window hooks are plain functions of the session, so it is
/// kept for the process: one person per running app.
static MESH: std::sync::Mutex<Option<CraftMesh>> = std::sync::Mutex::new(None);

fn with_mesh<T>(f: impl FnOnce(&mut CraftMesh) -> Result<T, String>) -> Result<T, String> {
    let mut guard = MESH.lock().map_err(|e| e.to_string())?;
    let mesh = guard.as_mut().ok_or_else(|| "this instance has no mesh identity yet".to_string())?;
    f(mesh)
}

fn install_mesh(mesh: CraftMesh) {
    if let Ok(mut guard) = MESH.lock() {
        *guard = Some(mesh);
    }
}

/// The startup default: the person's identity on this device, so the Share window's realm row
/// works before any workbook is shared.
pub fn ensure_default_identity(session: &mut Session) -> Result<(), String> {
    install_mesh(CraftMesh::default_identity()?);
    realm_refresh(session)?;
    Ok(())
}

/// The person's display name, as the Sign in window shows and edits it.
pub fn identity_name(_session: &mut Session) -> Result<serde_json::Value, String> {
    with_mesh(|mesh| Ok(mesh.identity_name()))
}

/// Sets the person's display name (persisted beside the key).
pub fn set_identity_name(_session: &mut Session, name: &str) -> Result<serde_json::Value, String> {
    with_mesh(|mesh| mesh.set_identity_name(name))
}

/// Reads the stored membership into the session and returns it.
pub fn realm_refresh(session: &mut Session) -> Result<serde_json::Value, String> {
    let status = with_mesh(|mesh| Ok(mesh.realm_status()))?;
    session.collab_set_realm(status.clone());
    Ok(status)
}

/// Starts the realm's citizen sign-in: the link the person opens or scans.
pub fn sign_in(_session: &mut Session) -> Result<serde_json::Value, String> {
    with_mesh(CraftMesh::sign_in)
}

/// Polls the waiting sign-in; a membership answer replaces the session's realm status.
pub fn sign_in_check(session: &mut Session) -> Result<serde_json::Value, String> {
    let status = with_mesh(CraftMesh::sign_in_check)?;
    if status.get("signedIn").is_some() {
        session.collab_set_realm(status.clone());
    }
    Ok(status)
}

/// Reads the config at `path` and shares or joins the active workbook.
pub fn attach(session: &mut Session, path: &str) -> Result<(), String> {
    let (mesh, shared) = CraftMesh::attach(path, &[])?;
    install_mesh(mesh);
    let _ = realm_refresh(session);
    session.collab_install(shared).map_err(|e| e.to_string())
}

/// Joins through an invite line (the Share window) into a new blank workbook: the person's
/// default identity, as `name`.
pub fn join_from_invite(session: &mut Session, invite_line: &str, name: &str) -> Result<(), String> {
    let (mesh, shared) = CraftMesh::join_invite(invite_line, name)?;
    install_mesh(mesh);
    session.new_workbook();
    session.collab_install(shared).map_err(|e| e.to_string())
}
