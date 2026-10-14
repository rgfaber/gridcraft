//! Agent commands for a workbook shared over the mesh: status, invites, the door's queue,
//! revocation, contacts and the realm membership. The Share window does the same through the
//! shared session; these let scripts and agents drive it.

use serde_json::{Value as Json, json};

use super::{always, bad, bool_param, cmd, str_param};
use crate::{CommandSpec, EngineError, Result, Session};

/// The longest an invite may live.
const MAX_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "collab.stats", "Collaboration Status", [], None, "{}", always, |s, _| Ok(s.collab_stats())),
        cmd!(query "collab.invite", "Invite to Workbook", [], None, "{realm, realmKey?, seed, profile?, role?: \"editor\"|\"viewer\", ttlMs?, realmMembers?: bool, forNode?, forName?}", always, invite),
        cmd!(query "collab.approve", "Approve a Joiner", [], None, "{node}", always, approve),
        cmd!(query "collab.revoke", "Revoke a Holder", [], None, "{node}", always, revoke),
        cmd!(query "collab.pending", "Pending Join Requests", [], None, "{}", always, pending),
        cmd!(query "collab.contacts", "List Contacts", [], None, "{}", always, contacts),
        cmd!(query "collab.forgetContact", "Forget a Contact", [], None, "{node}", always, forget_contact),
        cmd!(query "collab.realm", "Realm Membership", [], None, "{}", always, |s, _| Ok(s.collab_realm())),
    ]
}

fn shared(s: &mut Session) -> Result<&mut craft_collab::CollabSession> {
    s.collab_session_mut().ok_or_else(|| EngineError::Other("no workbook is shared".into()))
}

fn req<'a>(cmd: &str, p: &'a Json, key: &str) -> Result<&'a str> {
    str_param(p, key).ok_or_else(|| bad(cmd, format!("missing `{key}`")))
}

/// The host mints an invite line: the mesh coordinates to share (`realm`, `seed`) plus a
/// host-signed token, or for a realm invite none (members of the shared realm are admitted).
fn invite(s: &mut Session, p: &Json) -> Result<Json> {
    let share = craft_collab::ShareContext {
        realm: req("collab.invite", p, "realm")?.to_string(),
        realm_key: str_param(p, "realmKey").unwrap_or("").to_string(),
        seed: req("collab.invite", p, "seed")?.to_string(),
        profile: str_param(p, "profile").unwrap_or("pq_hybrid").to_string(),
    };
    let role = match str_param(p, "role").unwrap_or("editor") {
        "editor" => craft_collab::Role::Editor,
        "viewer" => craft_collab::Role::Viewer,
        other => return Err(bad("collab.invite", format!("unknown role `{other}` (editor or viewer)"))),
    };
    let ttl = p.get("ttlMs").and_then(Json::as_u64).unwrap_or(24 * 60 * 60 * 1_000).min(MAX_TTL_MS);
    let host_realm = s.collab_realm().get("signedIn").filter(|v| **v == true).and(s.collab_realm()["realm"].as_str().map(str::to_string));
    let c = shared(s)?;
    let line = if let Some(node) = str_param(p, "forNode") {
        c.mint_invite_for(Some(&share), host_realm.as_deref(), node, str_param(p, "forName").unwrap_or(""), role, ttl)
    } else if bool_param(p, "realmMembers").unwrap_or(false) {
        c.mint_realm_invite(Some(&share), host_realm.as_deref(), ttl)
    } else {
        c.mint_invite(Some(&share), host_realm.as_deref(), role, ttl)
    }
    .map_err(EngineError::Other)?;
    Ok(json!({"invite": line}))
}

fn approve(s: &mut Session, p: &Json) -> Result<Json> {
    let node = req("collab.approve", p, "node")?.to_string();
    shared(s)?.approve(&node).map_err(EngineError::Other)?;
    Ok(json!({"approved": node}))
}

fn revoke(s: &mut Session, p: &Json) -> Result<Json> {
    let node = req("collab.revoke", p, "node")?.to_string();
    shared(s)?.revoke(&node).map_err(EngineError::Other)?;
    Ok(json!({"revoked": node}))
}

fn pending(s: &mut Session, _: &Json) -> Result<Json> {
    let list = s.collab_session().map(craft_collab::CollabSession::pending).unwrap_or_default();
    Ok(json!({"pending": list.into_iter().map(|(node, name)| json!({"node": node, "name": name})).collect::<Vec<_>>()}))
}

fn contacts(s: &mut Session, _: &Json) -> Result<Json> {
    let list = s.collab_session().map(craft_collab::CollabSession::contacts).unwrap_or_default();
    Ok(json!({
        "contacts": list.into_iter().map(|c| json!({"node": c.node, "name": c.name, "lastSeenMs": c.last_seen_ms, "realm": c.realm})).collect::<Vec<_>>(),
    }))
}

fn forget_contact(s: &mut Session, p: &Json) -> Result<Json> {
    let node = req("collab.forgetContact", p, "node")?.to_string();
    Ok(json!({"forgotten": shared(s)?.forget_contact(&node)}))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Session;

    #[test]
    fn collab_commands_refuse_cleanly_when_nothing_is_shared() {
        let mut s = Session::new();
        s.new_workbook();
        assert!(s.execute("collab.invite", json!({"realm": "aa", "seed": "s"})).is_err());
        assert!(s.execute("collab.approve", json!({"node": "bb"})).is_err());
        assert!(s.execute("collab.approve", json!({})).is_err(), "node is required");
        assert_eq!(s.execute("collab.pending", json!({})).unwrap()["pending"].as_array().map(Vec::len), Some(0));
        assert_eq!(s.execute("collab.stats", json!({})).unwrap()["active"], false);
        assert_eq!(s.execute("collab.realm", json!({})).unwrap()["signedIn"], false);
        s.collab_set_realm(json!({"signedIn": true, "realm": "io.macula"}));
        assert_eq!(s.execute("collab.realm", json!({})).unwrap()["realm"], "io.macula");
    }
}
