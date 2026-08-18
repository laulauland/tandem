//! Bearer tokens and the writer role, from the outside.
//!
//! Three things are checked here, all of them over real HTTP against a real
//! `tandem serve`:
//!
//! * Nothing is readable without a token, and the admin token is the only one
//!   that mints others.
//! * A workspace token's authority is the diff its publish makes to the view:
//!   its own namespace yes, `main` and somebody else's workspace no.
//! * The writer role is claimed, renewed, contested and handed over when the
//!   claim behind it runs out.

use std::time::Duration;

use crate::common;

/// `POST` a JSON body with a bearer, and hand back the status and the body.
fn post_json(
    addr: &str,
    token: &str,
    path: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let response = common::http_client()
        .post(common::api_url(addr, path))
        .bearer_auth(token)
        .json(&body)
        .send()
        .unwrap_or_else(|err| panic!("POST {path}: {err}"));
    let status = response.status().as_u16();
    let text = response.text().expect("read the body");
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    (status, json)
}

/// A workspace-scoped token for `workspace`, minted by the admin token.
fn mint(addr: &str, admin: &str, workspace: &str, ttl_seconds: Option<u64>) -> String {
    let mut body = serde_json::json!({ "workspaceId": workspace });
    if let Some(ttl) = ttl_seconds {
        body["ttlSeconds"] = serde_json::json!(ttl);
    }
    let (status, minted) = post_json(addr, admin, "/api/tokens", body);
    assert_eq!(status, 200, "minting a token for {workspace}: {minted}");
    assert_eq!(minted["workspaceId"], workspace);
    minted["token"]
        .as_str()
        .unwrap_or_else(|| panic!("no token in {minted}"))
        .to_string()
}

// ─── Authentication ───────────────────────────────────────────────────────────

#[test]
fn every_endpoint_wants_a_bearer_and_refuses_a_wrong_one() {
    let fx = common::ServerFixture::start();
    let client = common::http_client();

    for path in ["/api/info", "/api/heads", "/api/events"] {
        let bare = client
            .get(common::api_url(&fx.addr, path))
            .timeout(Duration::from_secs(5))
            .send()
            .expect("send without a token");
        assert_eq!(
            bare.status().as_u16(),
            401,
            "GET {path} answered a caller with no token"
        );

        let wrong = client
            .get(common::api_url(&fx.addr, path))
            .bearer_auth("tdma_not-the-one")
            .timeout(Duration::from_secs(5))
            .send()
            .expect("send with a wrong token");
        assert_eq!(
            wrong.status().as_u16(),
            401,
            "GET {path} answered a caller with a made-up token"
        );
    }

    // And the same request with the real token is fine, so the 401s above are
    // about the token rather than about the endpoint.
    let ok = common::api_get(&fx.addr, fx.token(), "/api/info");
    assert!(ok.status().is_success());
}

#[test]
fn only_the_admin_token_mints_workspace_tokens() {
    let fx = common::ServerFixture::start();

    let agent_a = mint(&fx.addr, fx.token(), "agent-a", None);
    assert!(
        agent_a.starts_with("tdmw_"),
        "a workspace token should say what it is: {agent_a}"
    );

    // The minted token is good for reading.
    let info = common::api_get(&fx.addr, &agent_a, "/api/info");
    assert!(info.status().is_success());

    // It is not good for minting.
    let (status, body) = post_json(
        &fx.addr,
        &agent_a,
        "/api/tokens",
        serde_json::json!({ "workspaceId": "agent-b" }),
    );
    assert_eq!(status, 403, "a workspace token minted a token: {body}");

    // An expired token authorizes nothing at all.
    let expired = mint(&fx.addr, fx.token(), "agent-a", Some(0));
    let refused = common::http_client()
        .get(common::api_url(&fx.addr, "/api/info"))
        .bearer_auth(&expired)
        .send()
        .expect("send with an expired token");
    assert_eq!(refused.status().as_u16(), 401, "an expired token was taken");
}

// ─── What a workspace token may publish ───────────────────────────────────────

/// The authority matrix, driven through jj: the same bookmark command three
/// times, differing only in the name it uses.
#[test]
fn a_workspace_token_publishes_in_its_own_namespace_and_nowhere_else() {
    let fx = common::ServerFixture::start();
    let workspace = fx.init_workspace("agent-a", Some("agent-a"));
    let home = fx.home.clone();

    // Something to point a bookmark at.
    std::fs::write(workspace.join("a.txt"), b"agent-a was here\n").unwrap();
    common::assert_ok(
        &common::run_tandem_in(&workspace, &["describe", "-m", "agent-a work"], &home),
        "describe",
    );

    // Its own namespace: allowed.
    common::assert_ok(
        &common::run_tandem_in(
            &workspace,
            &["bookmark", "create", "agent-a/task-42", "-r", "@"],
            &home,
        ),
        "a bookmark in its own namespace",
    );

    // `main`: refused, and the refusal says why.
    let main = common::run_tandem_in(
        &workspace,
        &["bookmark", "create", "main", "-r", "@"],
        &home,
    );
    assert!(
        !main.status.success(),
        "a workspace token moved main\nstdout:\n{}\nstderr:\n{}",
        common::stdout_str(&main),
        common::stderr_str(&main)
    );
    let stderr = common::stderr_str(&main);
    assert!(
        stderr.contains("403") && stderr.contains("main"),
        "the refusal should name the bookmark it refused:\n{stderr}"
    );

    // Somebody else's namespace: refused too.
    let foreign = common::run_tandem_in(
        &workspace,
        &["bookmark", "create", "agent-b/task-1", "-r", "@"],
        &home,
    );
    assert!(
        !foreign.status.success(),
        "a workspace token wrote in another workspace's namespace\nstdout:\n{}\nstderr:\n{}",
        common::stdout_str(&foreign),
        common::stderr_str(&foreign)
    );

    // The one that was allowed is the one that is there.
    let list = common::run_tandem_in(&workspace, &["bookmark", "list"], &home);
    common::assert_ok(&list, "bookmark list");
    let text = common::stdout_str(&list);
    assert!(text.contains("agent-a/task-42"), "bookmarks:\n{text}");
    assert!(!text.contains("agent-b/task-1"), "bookmarks:\n{text}");
}

#[test]
fn a_workspace_token_may_not_publish_as_another_workspace() {
    let fx = common::ServerFixture::start();
    let agent_a = mint(&fx.addr, fx.token(), "agent-a", None);

    let head = common::api_get(&fx.addr, fx.token(), "/api/heads");
    let heads: serde_json::Value = head.json().expect("decode /api/heads");
    let version = heads["version"].as_u64().expect("version");
    let head_id = heads["heads"][0].as_str().expect("a head").to_string();

    let response = common::http_client()
        .post(common::api_url(&fx.addr, "/api/heads"))
        .bearer_auth(&agent_a)
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&serde_json::json!({
            "oldIds": [head_id],
            "newId": head_id,
            "workspaceId": "agent-b",
        }))
        .send()
        .expect("POST /api/heads");
    let status = response.status().as_u16();
    let body = response.text().unwrap_or_default();
    assert_eq!(
        status, 403,
        "agent-a published under agent-b's name: {body}"
    );
}

// ─── The publish check, from the wrong end ────────────────────────────────────

/// `POST` raw bytes with a bearer, and hand back the status and one header.
fn post_bytes(
    addr: &str,
    token: &str,
    path: &str,
    body: Vec<u8>,
    id_header: &str,
) -> (u16, Option<String>) {
    let response = common::http_client()
        .post(common::api_url(addr, path))
        .bearer_auth(token)
        .body(body)
        .send()
        .unwrap_or_else(|err| panic!("POST {path}: {err}"));
    let status = response.status().as_u16();
    let id = response
        .headers()
        .get(id_header)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    (status, id)
}

/// The view of the server's first operation head, decoded.
fn head_view(addr: &str, token: &str) -> (u64, String, jj_lib::op_store::View) {
    use prost::Message as _;

    let heads: serde_json::Value = common::api_get(addr, token, "/api/heads")
        .json()
        .expect("decode /api/heads");
    let version = heads["version"].as_u64().expect("version");
    let head_id = heads["heads"][0].as_str().expect("a head").to_string();

    let op_bytes = common::api_get(addr, token, &format!("/api/ops/{head_id}"))
        .bytes()
        .expect("read the head operation");
    let op = jj_lib::protos::simple_op_store::Operation::decode(&*op_bytes)
        .expect("decode the head operation");
    let view_hex = jj_tandem::hex::to_hex(&op.view_id);

    let view_bytes = common::api_get(addr, token, &format!("/api/views/{view_hex}"))
        .bytes()
        .expect("read the head view");
    let view_proto =
        jj_lib::protos::simple_op_store::View::decode(&*view_bytes).expect("decode the head view");
    let view = jj_tandem::proto_convert::view_from_proto(view_proto).expect("convert the view");

    (version, head_id, view)
}

/// Write a view, then an operation carrying it, and hand back both ids.
///
/// Neither route asks what the caller is entitled to, because neither route
/// changes what the server serves — this is what makes the base for the
/// publish check something the server has to choose for itself.
fn fabricate(
    addr: &str,
    token: &str,
    view: &jj_lib::op_store::View,
    parents: Vec<String>,
    description: &str,
) -> String {
    use prost::Message as _;

    let view_proto = jj_tandem::proto_convert::view_to_proto(view);
    let (status, view_id) = post_bytes(
        addr,
        token,
        "/api/views",
        view_proto.encode_to_vec(),
        jj_tandem::wire::HEADER_VIEW_ID,
    );
    assert_eq!(status, 200, "a workspace token could not write a view");
    let view_id = view_id.expect("the view id header");

    let mut metadata = jj_lib::protos::simple_op_store::OperationMetadata {
        description: description.to_string(),
        ..Default::default()
    };
    metadata
        .tags
        .insert("test".to_string(), "forged".to_string());

    let op_proto = jj_lib::protos::simple_op_store::Operation {
        view_id: jj_tandem::hex::from_hex(&view_id).expect("view id is hex"),
        parents: parents
            .iter()
            .map(|hex| jj_tandem::hex::from_hex(hex).expect("parent id is hex"))
            .collect(),
        metadata: Some(metadata),
        commit_predecessors: vec![],
        stores_commit_predecessors: false,
    };
    let (status, op_id) = post_bytes(
        addr,
        token,
        "/api/ops",
        op_proto.encode_to_vec(),
        jj_tandem::wire::HEADER_OPERATION_ID,
    );
    assert_eq!(
        status, 200,
        "a workspace token could not write an operation"
    );
    op_id.expect("the operation id header")
}

/// Publish `new_id` as the head that replaces `old_id`.
fn publish(
    addr: &str,
    token: &str,
    version: u64,
    old_id: &str,
    new_id: &str,
    workspace: &str,
) -> (u16, String) {
    let response = common::http_client()
        .post(common::api_url(addr, "/api/heads"))
        .bearer_auth(token)
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&serde_json::json!({
            "oldIds": [old_id],
            "newId": new_id,
            "workspaceId": workspace,
        }))
        .send()
        .expect("POST /api/heads");
    let status = response.status().as_u16();
    (status, response.text().unwrap_or_default())
}

/// The op-store routes accept anything well-formed, so an operation is not
/// evidence of anything until the server has published it.
///
/// The attack this pins: agent-a writes an operation whose view moves `main`
/// (or somebody else's working copy), never publishes *it*, and publishes a
/// child of it instead — hoping the check reads its base out of the parent and
/// concludes that `main` was already there. A parent only counts as a base once
/// the server has published it, so the publish is refused at the parent and
/// never gets as far as reading its view.
#[test]
fn a_fabricated_parent_operation_does_not_widen_what_a_token_may_publish() {
    use jj_lib::op_store::RefTarget;
    use jj_lib::ref_name::{RefNameBuf, WorkspaceNameBuf};

    let fx = common::ServerFixture::start();
    let agent_a_dir = fx.init_workspace("agent-a", Some("agent-a"));
    let agent_b_dir = fx.init_workspace("agent-b", Some("agent-b"));
    let home = fx.home.clone();

    // Both agents have a commit, so there is something to point a bookmark at
    // and something of agent-b's to steal.
    for (dir, text) in [(&agent_a_dir, "a\n"), (&agent_b_dir, "b\n")] {
        std::fs::write(dir.join("f.txt"), text).unwrap();
        common::assert_ok(
            &common::run_tandem_in(dir, &["describe", "-m", "work"], &home),
            "describe",
        );
    }

    let agent_a = mint(&fx.addr, fx.token(), "agent-a", None);

    // ── Moving `main` through a forged parent ──
    let (version, head_id, view) = head_view(&fx.addr, &agent_a);
    let target = view
        .wc_commit_ids
        .get(&WorkspaceNameBuf::from("agent-a".to_string()))
        .expect("agent-a has a working-copy commit")
        .clone();

    let mut moves_main = view.clone();
    moves_main.local_bookmarks.insert(
        RefNameBuf::from("main".to_string()),
        RefTarget::normal(target.clone()),
    );

    // The forged parent: a child of the real head, carrying the view that
    // moves `main`. Published on its own it would be refused, so it is not.
    let forged_parent = fabricate(
        &fx.addr,
        &agent_a,
        &moves_main,
        vec![head_id.clone()],
        "forged parent",
    );
    // And the operation actually published: a child of the forged one, whose
    // view says exactly what its parent's said.
    let child = fabricate(
        &fx.addr,
        &agent_a,
        &moves_main,
        vec![forged_parent.clone()],
        "child of a forged parent",
    );

    let (status, body) = publish(&fx.addr, &agent_a, version, &head_id, &child, "agent-a");
    assert_eq!(
        status, 403,
        "a forged parent operation let a workspace token move main: {body}"
    );
    assert!(
        body.contains(&forged_parent),
        "the refusal should name the parent it does not serve: {body}"
    );

    // ── Moving another workspace's pointer the same way ──
    let (version, head_id, view) = head_view(&fx.addr, &agent_a);
    let mut steals_agent_b = view.clone();
    steals_agent_b.wc_commit_ids.insert(
        WorkspaceNameBuf::from("agent-b".to_string()),
        target.clone(),
    );

    let forged_parent = fabricate(
        &fx.addr,
        &agent_a,
        &steals_agent_b,
        vec![head_id.clone()],
        "forged parent moving agent-b",
    );
    let child = fabricate(
        &fx.addr,
        &agent_a,
        &steals_agent_b,
        vec![forged_parent.clone()],
        "child of a forged parent moving agent-b",
    );

    let (status, body) = publish(&fx.addr, &agent_a, version, &head_id, &child, "agent-a");
    assert_eq!(
        status, 403,
        "a forged parent operation let a workspace token move agent-b's working copy: {body}"
    );
    assert!(
        body.contains(&forged_parent),
        "the refusal should name the parent it does not serve: {body}"
    );

    // ── And nothing of it stuck ──
    let list = common::run_tandem_in(&agent_a_dir, &["bookmark", "list", "--all-remotes"], &home);
    common::assert_ok(&list, "bookmark list");
    let text = common::stdout_str(&list);
    assert!(
        !text.contains("main"),
        "main exists after the forged publishes:\n{text}"
    );

    let (_, _, view) = head_view(&fx.addr, fx.token());
    assert!(
        !view
            .local_bookmarks
            .contains_key(&RefNameBuf::from("main".to_string())),
        "the served view has main after the forged publishes"
    );
    let agent_b_now = view
        .wc_commit_ids
        .get(&WorkspaceNameBuf::from("agent-b".to_string()))
        .cloned();
    assert!(
        agent_b_now.is_some() && agent_b_now.as_ref() != Some(&target),
        "agent-b's working copy is sitting on agent-a's commit"
    );
}

/// The other half of the forged-parent trick: superseding a head instead of
/// replacing it.
///
/// A publish does not have to name a head in `oldIds` to retire it. An
/// operation that *descends* from a head retires it in the reconcile, which
/// drops any head another head descends from. So a token can chain a forged
/// operation onto the real head and publish the forged operation's child: the
/// CAS retires nothing, and the reconcile settles on the child anyway.
///
/// What the child carries here is not a value it invented but the *absence* of
/// one — agent-b's working copy, simply left out. Nothing but the base decides
/// whether that reads as "deleted it" or as "never had it", which is why the
/// base may not include the empty repo for an operation like this one.
#[test]
fn a_forged_ancestor_does_not_let_a_token_drop_what_the_server_serves() {
    use jj_lib::ref_name::{RefNameBuf, WorkspaceNameBuf};

    let fx = common::ServerFixture::start();
    let agent_a_dir = fx.init_workspace("agent-a", Some("agent-a"));
    let agent_b_dir = fx.init_workspace("agent-b", Some("agent-b"));
    let home = fx.home.clone();

    std::fs::write(agent_a_dir.join("f.txt"), "a\n").unwrap();
    common::assert_ok(
        &common::run_tandem_in(&agent_a_dir, &["describe", "-m", "work"], &home),
        "describe",
    );
    common::assert_ok(
        &common::run_tandem_in(
            &agent_b_dir,
            &["bookmark", "create", "agent-b/keep-me", "-r", "@"],
            &home,
        ),
        "agent-b makes a bookmark of its own",
    );

    let agent_a = mint(&fx.addr, fx.token(), "agent-a", None);
    let (version, head_id, view) = head_view(&fx.addr, &agent_a);
    let agent_b = WorkspaceNameBuf::from("agent-b".to_string());
    let keep_me = RefNameBuf::from("agent-b/keep-me".to_string());
    assert!(
        view.wc_commit_ids.contains_key(&agent_b) && view.local_bookmarks.contains_key(&keep_me),
        "agent-b should have a pointer and a bookmark to lose"
    );

    // Everything of agent-b's, gone.
    let mut without_agent_b = view.clone();
    without_agent_b.wc_commit_ids.remove(&agent_b);
    without_agent_b.local_bookmarks.remove(&keep_me);

    let forged_ancestor = fabricate(
        &fx.addr,
        &agent_a,
        &without_agent_b,
        vec![head_id.clone()],
        "forged ancestor",
    );
    let child = fabricate(
        &fx.addr,
        &agent_a,
        &without_agent_b,
        vec![forged_ancestor],
        "child of a forged ancestor",
    );

    let (status, body) = publish(&fx.addr, &agent_a, version, &head_id, &child, "agent-a");
    assert_eq!(
        status, 403,
        "a forged ancestor let a workspace token drop agent-b's state: {body}"
    );

    // And it is all still served.
    let (_, _, view) = head_view(&fx.addr, fx.token());
    assert!(
        view.wc_commit_ids.contains_key(&agent_b),
        "agent-b's working copy is gone"
    );
    assert!(
        view.local_bookmarks.contains_key(&keep_me),
        "agent-b's bookmark is gone"
    );
}

/// A merge may carry a deletion forward, and this is what stops that from
/// becoming a way to delete anything.
///
/// Leaving a value out of a merge is how jj records a deletion made on one
/// side, so the check has to allow it — otherwise an honest client that merges
/// two operation heads is refused for propagating somebody else's `jj bookmark
/// delete`. The allowance is not "some parent lacks it", though, because a
/// token may name as a second parent any operation the server serves, and an
/// old enough one predates whatever it wants to delete. jj's own merge of that
/// pair would keep the bookmark: a side that never had a value does not vote
/// against it. So the server reads the merge base, and the base is what tells
/// a removal from a never-had.
///
/// Everything here is honest except the view: both parents are operations the
/// server published, the CAS names the real head, and the published view is
/// the head's own view with one foreign bookmark left out.
#[test]
fn a_parent_that_predates_a_bookmark_does_not_let_a_merge_delete_it() {
    use jj_lib::ref_name::RefNameBuf;

    let fx = common::ServerFixture::start();
    let agent_a_dir = fx.init_workspace("agent-a", Some("agent-a"));
    let agent_b_dir = fx.init_workspace("agent-b", Some("agent-b"));
    let home = fx.home.clone();

    std::fs::write(agent_a_dir.join("f.txt"), "a\n").unwrap();
    common::assert_ok(
        &common::run_tandem_in(&agent_a_dir, &["describe", "-m", "work"], &home),
        "describe",
    );

    // The operation that predates the bookmark. It is nobody's forgery: the
    // server published it, and it is still reachable from the head.
    let (_, predates_it, _) = head_view(&fx.addr, fx.token());

    common::assert_ok(
        &common::run_tandem_in(
            &agent_b_dir,
            &["bookmark", "create", "agent-b/keep-me", "-r", "@"],
            &home,
        ),
        "agent-b makes a bookmark of its own",
    );

    let agent_a = mint(&fx.addr, fx.token(), "agent-a", None);
    let (version, head_id, view) = head_view(&fx.addr, &agent_a);
    let keep_me = RefNameBuf::from("agent-b/keep-me".to_string());
    assert!(
        view.local_bookmarks.contains_key(&keep_me),
        "agent-b should have a bookmark to lose"
    );
    assert_ne!(
        predates_it, head_id,
        "the bookmark should have moved the head on"
    );

    let mut without_the_bookmark = view.clone();
    without_the_bookmark.local_bookmarks.remove(&keep_me);

    let merge = fabricate(
        &fx.addr,
        &agent_a,
        &without_the_bookmark,
        vec![predates_it, head_id.clone()],
        "a merge with a parent old enough to predate the bookmark",
    );

    let (status, body) = publish(&fx.addr, &agent_a, version, &head_id, &merge, "agent-a");
    assert_eq!(
        status, 403,
        "a parent that predates the bookmark let a token delete it: {body}"
    );
    assert!(
        body.contains("agent-b/keep-me"),
        "the refusal should name the bookmark: {body}"
    );

    let (_, _, view) = head_view(&fx.addr, fx.token());
    assert!(
        view.local_bookmarks.contains_key(&keep_me),
        "agent-b's bookmark is gone"
    );
}

/// The one operation that *is* measured against the empty repo — a brand-new
/// repo's first operation, which is what `tandem init` publishes — cannot take
/// anything away either, and this is why.
///
/// Such an operation descends from no current head, so the reconcile merges it
/// beside them instead of dropping them. Whatever it leaves out comes straight
/// back. The test publishes exactly that shape by hand and then reads the
/// served view.
#[test]
fn an_operation_forking_from_the_root_cannot_subtract_from_the_served_view() {
    use jj_lib::ref_name::WorkspaceNameBuf;

    let fx = common::ServerFixture::start();
    let agent_a_dir = fx.init_workspace("agent-a", Some("agent-a"));
    let _agent_b_dir = fx.init_workspace("agent-b", Some("agent-b"));
    let home = fx.home.clone();

    std::fs::write(agent_a_dir.join("f.txt"), "a\n").unwrap();
    common::assert_ok(
        &common::run_tandem_in(&agent_a_dir, &["describe", "-m", "work"], &home),
        "describe",
    );

    let agent_a = mint(&fx.addr, fx.token(), "agent-a", None);
    let (version, head_id, view) = head_view(&fx.addr, &agent_a);
    let agent_b = WorkspaceNameBuf::from("agent-b".to_string());
    let agent_b_was = view
        .wc_commit_ids
        .get(&agent_b)
        .expect("agent-b has a working copy")
        .clone();

    // jj's root operation: sixty-four zero bytes, the ancestor of everything.
    let root_operation = "0".repeat(128);
    let mut without_agent_b = view.clone();
    without_agent_b.wc_commit_ids.remove(&agent_b);

    let forked = fabricate(
        &fx.addr,
        &agent_a,
        &without_agent_b,
        vec![root_operation],
        "a new repo's first operation",
    );
    let (status, body) = publish(&fx.addr, &agent_a, version, &head_id, &forked, "agent-a");
    assert_eq!(
        status, 200,
        "a fork from the root operation was refused: {body}"
    );

    let (_, _, view) = head_view(&fx.addr, fx.token());
    assert_eq!(
        view.wc_commit_ids.get(&agent_b),
        Some(&agent_b_was),
        "the merge did not put agent-b's working copy back"
    );
}

// ─── The writer role ──────────────────────────────────────────────────────────

#[test]
fn the_writer_role_is_claimed_renewed_contested_and_handed_over() {
    let fx = common::ServerFixture::start();
    let agent_a = mint(&fx.addr, fx.token(), "agent-a", None);
    let path = "/api/workspaces/agent-a/writer";

    // Nobody holds it, so the first client does.
    let (status, held) = post_json(
        &fx.addr,
        &agent_a,
        path,
        serde_json::json!({ "holder": "daemon-1", "ttlSeconds": 1 }),
    );
    assert_eq!(status, 200, "the first claim: {held}");
    assert_eq!(held["workspaceId"], "agent-a");
    assert_eq!(held["holder"], "daemon-1");
    assert!(held["expiresInSeconds"].as_u64().is_some(), "{held}");

    // The same holder asking again is renewing.
    let (status, renewed) = post_json(
        &fx.addr,
        &agent_a,
        path,
        serde_json::json!({ "holder": "daemon-1", "ttlSeconds": 1 }),
    );
    assert_eq!(status, 200, "a renewal by the holder: {renewed}");

    // Anybody else asking while it is held is refused, and told who holds it.
    let (status, refused) = post_json(
        &fx.addr,
        &agent_a,
        path,
        serde_json::json!({ "holder": "daemon-2", "ttlSeconds": 1 }),
    );
    assert_eq!(status, 409, "a second client took a held role: {refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or_default()
            .contains("daemon-1"),
        "the refusal should name the holder: {refused}"
    );

    // Left unrenewed, the claim runs out and the next client takes it.
    std::thread::sleep(Duration::from_millis(1_400));
    let (status, taken) = post_json(
        &fx.addr,
        &agent_a,
        path,
        serde_json::json!({ "holder": "daemon-2", "ttlSeconds": 30 }),
    );
    assert_eq!(status, 200, "an expired role was not handed over: {taken}");
    assert_eq!(taken["holder"], "daemon-2");
}

#[test]
fn the_writer_role_of_another_workspace_is_not_claimable() {
    let fx = common::ServerFixture::start();
    let agent_a = mint(&fx.addr, fx.token(), "agent-a", None);

    let (status, refused) = post_json(
        &fx.addr,
        &agent_a,
        "/api/workspaces/agent-b/writer",
        serde_json::json!({ "holder": "daemon-1" }),
    );
    assert_eq!(
        status, 403,
        "agent-a claimed the writer role of agent-b: {refused}"
    );

    // The admin token speaks for every workspace, so it may.
    let (status, held) = post_json(
        &fx.addr,
        fx.token(),
        "/api/workspaces/agent-b/writer",
        serde_json::json!({ "holder": "the-integrator" }),
    );
    assert_eq!(status, 200, "the admin token was refused: {held}");
}

// ─── The word nobody uses ─────────────────────────────────────────────────────

/// The term is "writer role". Nothing a person or a client sees says "lease".
#[test]
fn nothing_user_facing_says_lease() {
    let fx = common::ServerFixture::start();
    let agent_a = mint(&fx.addr, fx.token(), "agent-a", None);

    let mut seen = String::new();

    for args in [
        vec!["--help"],
        vec!["serve", "--help"],
        vec!["init", "--help"],
        vec!["watch", "--help"],
        vec!["up", "--help"],
    ] {
        let out = common::run_tandem_in(fx.path(), &args, &fx.home);
        seen.push_str(&common::stdout_str(&out));
        seen.push_str(&common::stderr_str(&out));
    }

    // The writer-role endpoint, in both the granted and the refused answer.
    let (_, held) = post_json(
        &fx.addr,
        &agent_a,
        "/api/workspaces/agent-a/writer",
        serde_json::json!({ "holder": "daemon-1" }),
    );
    seen.push_str(&held.to_string());
    let (_, refused) = post_json(
        &fx.addr,
        &agent_a,
        "/api/workspaces/agent-a/writer",
        serde_json::json!({ "holder": "daemon-2" }),
    );
    seen.push_str(&refused.to_string());

    // The whole word, not the letters: "release" is a different word and an
    // honest one.
    let says_lease = seen
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphabetic())
        .any(|word| matches!(word, "lease" | "leases" | "leased" | "leasing"));
    assert!(!says_lease, "something user-facing says \"lease\":\n{seen}");
}
