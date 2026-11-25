//! What Drive says of one item beyond a listing: its parent, whether this
//! account may share it, and who may open it (`permissions.list`). Read only.

use pk_cli_core::{CliError, Reason};
use serde::{Deserialize, Serialize};

use super::api::Api;
use super::drive::ItemKind;
use super::files::{DriveFile, FIELDS};

/// The fields a [`ItemDetails`] read asks for.
const DETAIL_FIELDS: &str = ",trashed,ownedByMe,shared,inheritedPermissionsDisabled,\
capabilities(canShare)";

/// The fields a grant is read with.
const PERMISSION_FIELDS: &str = "nextPageToken,permissions(id,type,role,emailAddress,domain,\
allowFileDiscovery,expirationTime,deleted,pendingOwner,view,permissionDetails)";

/// One item, as Drive's metadata describes it ([`super::Drive::details`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ItemDetails {
    pub id: String,
    pub name: String,
    pub kind: ItemKind,
    /// The Drive id of the folder that holds it. `None` when Drive names
    /// none this sign-in can see, as for My Drive itself. An item shared with
    /// the account may have none.
    pub parent: Option<String>,
    pub size: Option<u64>,
    /// Unix seconds.
    pub modified: Option<i64>,
    pub md5: Option<String>,
    pub trashed: bool,
    /// Whether the signed-in account owns it.
    pub owned_by_me: Option<bool>,
    /// Whether it has been shared with anyone else.
    pub shared: Option<bool>,
    /// Whether the signed-in account may change who can open it.
    pub can_share: bool,
    /// A folder with inherited grants turned off ("limited access"): only
    /// its owner and the people added to it directly can open it.
    pub limited_access: bool,
    /// Reached through a shortcut, whose target this is.
    pub via_shortcut: bool,
}

/// One grant of access to an item ([`super::Drive::permissions`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Permission {
    /// Drive's id for the grant, the same for a person on every item.
    pub id: String,
    /// `user`, `group`, `domain` or `anyone`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `owner`, `writer`, `commenter` or `reader`.
    pub role: String,
    /// The account or group, for `user` and `group`.
    pub email: Option<String>,
    /// The Google Workspace domain, for `domain`.
    pub domain: Option<String>,
    /// For `domain` and `anyone`: whether the item shows up in search for
    /// that audience, or is reached only by its link.
    pub discoverable: Option<bool>,
    /// Drive's `expirationTime` (RFC 3339), when the grant ends.
    pub expires: Option<String>,
    /// The account behind the grant was deleted.
    pub deleted: bool,
    /// The account has been asked to take ownership.
    pub pending_owner: bool,
    /// `Some(true)`: the grant reaches the item from a folder above.
    /// `Some(false)`: it is on the item itself. `None`: nothing settles it.
    pub inherited: Option<bool>,
    /// The folder it comes from, when Drive names it or the item's own
    /// folder holds the same grant.
    pub inherited_from: Option<String>,
    /// Set when the grant is only for a view, not for the item's content
    /// (`metadata` on a folder with limited access).
    pub view: Option<String>,
}

/// An item and everyone who may open it ([`super::Drive::permissions`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Sharing {
    pub item: ItemDetails,
    pub permissions: Vec<Permission>,
}

/// Whether an error is Drive's refusal of the item itself: a 404, or a 403
/// the sign-in's scopes do not explain. Anything else (a lost sign-in, a
/// missing scope, a rate limit) stands, with its own remedy.
pub(super) fn refused(e: &CliError) -> bool {
    e.exit_code() == 4
        || (e.exit_code() == 3 && e.reason() == Some(Reason::Forbidden) && e.remedy().is_none())
}

impl Permission {
    /// A link-sharing grant: anyone with the link, or anyone in a domain.
    pub fn by_link(&self) -> bool {
        matches!(self.kind.as_str(), "anyone" | "domain")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Details {
    #[serde(flatten)]
    file: DriveFile,
    #[serde(default)]
    trashed: bool,
    #[serde(default)]
    owned_by_me: Option<bool>,
    #[serde(default)]
    shared: Option<bool>,
    #[serde(default)]
    inherited_permissions_disabled: bool,
    #[serde(default)]
    capabilities: Capabilities,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Capabilities {
    #[serde(default)]
    can_share: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Grant {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    role: String,
    #[serde(default)]
    email_address: Option<String>,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    allow_file_discovery: Option<bool>,
    #[serde(default)]
    expiration_time: Option<String>,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    pending_owner: bool,
    #[serde(default)]
    view: Option<String>,
    #[serde(default)]
    permission_details: Vec<GrantDetail>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrantDetail {
    #[serde(default)]
    inherited: Option<bool>,
    #[serde(default)]
    inherited_from: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Grants {
    #[serde(default)]
    permissions: Vec<Grant>,
    #[serde(default)]
    next_page_token: Option<String>,
}

/// How much access a role gives, so a grant on the folder above can be told
/// from a higher one given on the item.
fn rank(role: &str) -> Option<u8> {
    Some(match role {
        "reader" => 1,
        "commenter" => 2,
        "writer" => 3,
        "fileOrganizer" => 4,
        "organizer" => 5,
        "owner" => 6,
        _ => return None,
    })
}

impl Grant {
    /// Where Drive's own `permissionDetails` say the grant comes from:
    /// whether it is inherited, and from which folder. `None` when Drive
    /// gives no details, or one does not say.
    fn inherited(&self) -> Option<(bool, Option<String>)> {
        if self.permission_details.is_empty() {
            return None;
        }
        let mut direct = false;
        let mut from = None;
        for d in &self.permission_details {
            if d.inherited? {
                from = from.or_else(|| d.inherited_from.clone());
            } else {
                direct = true;
            }
        }
        // Given on the item as well: it would stand without the folder above.
        Some((!direct, if direct { None } else { from }))
    }

    fn into_permission(self) -> Permission {
        let (inherited, inherited_from) = match self.inherited() {
            Some((inherited, from)) => (Some(inherited), from),
            None => (None, None),
        };
        Permission {
            id: self.id,
            kind: self.kind,
            role: self.role,
            email: self.email_address,
            domain: self.domain,
            discoverable: self.allow_file_discovery,
            expires: self.expiration_time,
            deleted: self.deleted,
            pending_owner: self.pending_owner,
            inherited,
            inherited_from,
            view: self.view,
        }
    }
}

/// Settle where each grant comes from that Drive left open, from the folder
/// above. A folder's grants reach everything in it, and one it holds at a
/// role at least as high as the item's is the one the item's comes from; a
/// higher role on the item was given there. With limited access nothing is
/// inherited, except the metadata view Drive adds. `parent` is `None` when
/// the folder above, or who may open it, is not visible.
fn settle(item: &ItemDetails, permissions: &mut [Permission], parent: Option<&[Permission]>) {
    for p in permissions.iter_mut().filter(|p| p.inherited.is_none()) {
        if item.limited_access {
            p.inherited = Some(p.view.is_some());
        } else if p.role == "owner" {
            // Ownership is never inherited.
            p.inherited = Some(false);
        } else if let Some(parent) = parent {
            let reaches = |q: &&Permission| {
                q.id == p.id
                    && q.view.is_none()
                    && rank(&q.role)
                        .zip(rank(&p.role))
                        .is_some_and(|(above, mine)| above >= mine)
            };
            let from_above = parent.iter().any(|q| reaches(&q));
            p.inherited = Some(from_above);
            p.inherited_from = from_above.then(|| item.parent.clone()).flatten();
        }
    }
}

impl Api {
    /// One item's metadata, `None` when Drive does not know it (or this
    /// sign-in may not see it).
    pub(crate) fn details(
        &self,
        what: &str,
        id: &str,
        via_shortcut: bool,
    ) -> Result<Option<ItemDetails>, CliError> {
        let url = format!("{}/files/{}", self.endpoints.api, id);
        let answer = self.call(what, true, &[404], &|http| {
            http.get(&url)
                .query(&[("fields", format!("{FIELDS}{DETAIL_FIELDS}"))])
        })?;
        if answer.status == 404 {
            return Ok(None);
        }
        let d: Details = answer.json(what)?;
        let f = &d.file;
        let kind = if f.is_folder() {
            ItemKind::Folder
        } else if f.is_blob() {
            ItemKind::File
        } else {
            ItemKind::Document
        };
        Ok(Some(ItemDetails {
            id: f.id.clone(),
            name: f.name.clone(),
            kind,
            parent: f.parents.first().cloned(),
            size: f.size(),
            modified: f.modified(),
            md5: f.md5(),
            trashed: d.trashed,
            owned_by_me: d.owned_by_me,
            shared: d.shared,
            can_share: d.capabilities.can_share,
            limited_access: d.inherited_permissions_disabled,
            via_shortcut,
        }))
    }

    /// Every grant on the item `id`, where Drive's own answer says where
    /// each comes from.
    fn grants(&self, what: &str, id: &str) -> Result<Vec<Permission>, CliError> {
        let url = format!("{}/files/{}/permissions", self.endpoints.api, id);
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let answer = self.call(what, true, &[], &|http| {
                let mut req = http
                    .get(&url)
                    .query(&[("fields", PERMISSION_FIELDS), ("pageSize", "100")]);
                if let Some(t) = &token {
                    req = req.query(&[("pageToken", t.as_str())]);
                }
                req
            })?;
            let page: Grants = answer.json(what)?;
            out.extend(page.permissions.into_iter().map(Grant::into_permission));
            match page.next_page_token {
                Some(t) if !t.is_empty() => token = Some(t),
                _ => return Ok(out),
            }
        }
    }

    /// Everyone who may open `item`, sorted by role (the owner first).
    pub(crate) fn sharing(&self, what: &str, item: ItemDetails) -> Result<Sharing, CliError> {
        let mut permissions = self.grants(what, &item.id)?;
        let open = |p: &&Permission| p.inherited.is_none() && p.role != "owner";
        let parent = match &item.parent {
            Some(id) if !item.limited_access && permissions.iter().any(|p| open(&p)) => {
                // A folder this sign-in may not read leaves where its grants
                // reach unknown. Any other failure, a lost sign-in included,
                // is the caller's to see.
                match self.grants(what, id) {
                    Ok(list) => Some(list),
                    Err(e) if refused(&e) => None,
                    Err(e) => return Err(e),
                }
            }
            _ => None,
        };
        settle(&item, &mut permissions, parent.as_deref());
        permissions.sort_by(|a, b| {
            rank(&b.role).cmp(&rank(&a.role)).then_with(|| {
                (&a.kind, &a.email, &a.domain, &a.id).cmp(&(&b.kind, &b.email, &b.domain, &b.id))
            })
        });
        Ok(Sharing { item, permissions })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(json: &str) -> Grant {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn details_of_a_grant_say_whether_it_is_inherited() {
        let mixed = grant(
            r#"{"id":"1","type":"user","role":"writer","permissionDetails":[
                {"inherited":true,"inheritedFrom":"1Above"},{"inherited":false}]}"#,
        );
        // Given on the item too, so it is direct, from no folder.
        let mixed = mixed.into_permission();
        assert_eq!(mixed.inherited, Some(false));
        assert_eq!(mixed.inherited_from, None);
        let above = grant(
            r#"{"id":"1","type":"user","role":"reader","permissionDetails":[
                {"inherited":true,"inheritedFrom":"1Above"}]}"#,
        );
        let p = above.into_permission();
        assert_eq!(p.inherited, Some(true));
        assert_eq!(p.inherited_from.as_deref(), Some("1Above"));
        let silent = grant(r#"{"id":"1","type":"user","role":"reader"}"#);
        assert_eq!(silent.into_permission().inherited, None);
        assert_eq!(rank("owner"), Some(6));
        assert_eq!(rank("somethingNew"), None);
    }
}
