//! Integration tests: `?q=` substring search on `GET /api/queries` and `GET /api/dashboards`.

mod common;
use common::ok_json;
use common::spawn_app;

fn names_in(list: &serde_json::Value) -> Vec<String> {
    let results = if list.is_array() {
        list.as_array().unwrap().clone()
    } else {
        list["results"].as_array().unwrap().clone()
    };
    results
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect()
}

async fn search_queries(client: &reqwest::Client, address: &str, q: &str) -> Vec<String> {
    let resp = client
        .get(format!("{address}/api/queries"))
        .query(&[("q", q)])
        .send()
        .await
        .unwrap();
    names_in(&ok_json(resp, &format!("search queries q={q}")).await)
}

async fn search_dashboards(client: &reqwest::Client, address: &str, q: &str) -> Vec<String> {
    let resp = client
        .get(format!("{address}/api/dashboards"))
        .query(&[("q", q)])
        .send()
        .await
        .unwrap();
    names_in(&ok_json(resp, &format!("search dashboards q={q}")).await)
}

/// `?q=` on `/api/queries` matches substrings/prefixes (not just whole tokenized words), is
/// case-insensitive, and covers name/description/query-text.
#[tokio::test]
async fn query_search_matches_substrings_across_indexed_fields() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "search-test-pg", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    // Matches only by name.
    ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Monthly Revenue",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create Monthly Revenue query",
    )
    .await;

    // Matches only by description.
    ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Unrelated Name",
                "description": "tracks weekly signups",
                "query": "SELECT 2",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create description-matched query",
    )
    .await;

    // Matches only by query text.
    ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Another Query",
                "query": "SELECT * FROM churn_events",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query-text-matched query",
    )
    .await;

    // "Reve" is a clean prefix of "Revenue" — plainto_tsquery stems to 'reve' vs 'revenu' and
    // never matches; ILIKE substring matching does.
    for term in ["Reve", "even", "REVENUE"] {
        let names = search_queries(&client, &app.address, term).await;
        assert!(
            names.contains(&"Monthly Revenue".to_string()),
            "term {term:?} should match 'Monthly Revenue', got {names:?}"
        );
    }

    let by_description = search_queries(&client, &app.address, "signups").await;
    assert!(
        by_description.contains(&"Unrelated Name".to_string()),
        "description-field match failed, got {by_description:?}"
    );

    let by_query_text = search_queries(&client, &app.address, "churn_events").await;
    assert!(
        by_query_text.contains(&"Another Query".to_string()),
        "query-text-field match failed, got {by_query_text:?}"
    );

    // Empty q is treated as no filter.
    let unfiltered = search_queries(&client, &app.address, "").await;
    assert!(
        unfiltered.len() >= 3,
        "empty q should return the unfiltered list, got {unfiltered:?}"
    );
}

/// A literal `%`/`_` typed by the user must not act as a LIKE wildcard.
#[tokio::test]
async fn query_search_escapes_like_wildcards() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    let ds = ok_json(
        client
            .post(format!("{}/api/data_sources", app.address))
            .json(&serde_json::json!({ "name": "wildcard-test-pg", "type": "pg", "options": {} }))
            .send()
            .await
            .unwrap(),
        "create data source",
    )
    .await;
    let ds_id = ds["id"].as_i64().unwrap();

    ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "Growth % Target",
                "query": "SELECT 1",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query with literal percent in name",
    )
    .await;
    ok_json(
        client
            .post(format!("{}/api/queries", app.address))
            .json(&serde_json::json!({
                "data_source_id": ds_id,
                "name": "No Special Chars Here",
                "query": "SELECT 2",
                "options": {}
            }))
            .send()
            .await
            .unwrap(),
        "create query without percent",
    )
    .await;

    // If `%` were an unescaped LIKE wildcard this would match every query, not just the one
    // that actually contains a literal '%' character.
    let names = search_queries(&client, &app.address, "%").await;
    assert_eq!(
        names,
        vec!["Growth % Target".to_string()],
        "literal % must only match rows with an actual % character, got {names:?}"
    );
}

/// `?q=` on `/api/dashboards` matches substrings across name and tags.
#[tokio::test]
async fn dashboard_search_matches_substrings_across_name_and_tags() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;
    let client = app.admin_client().await;

    ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Monthly Revenue" }))
            .send()
            .await
            .unwrap(),
        "create Monthly Revenue dashboard",
    )
    .await;

    let tagged = ok_json(
        client
            .post(format!("{}/api/dashboards", app.address))
            .json(&serde_json::json!({ "name": "Unrelated Dashboard Name" }))
            .send()
            .await
            .unwrap(),
        "create tag-matched dashboard",
    )
    .await;
    let tagged_slug = tagged["slug"].as_str().unwrap().to_string();
    ok_json(
        client
            .post(format!("{}/api/dashboards/{tagged_slug}", app.address))
            .json(&serde_json::json!({ "tags": ["quarterly-finance"] }))
            .send()
            .await
            .unwrap(),
        "tag the second dashboard",
    )
    .await;

    for term in ["Reve", "even", "REVENUE"] {
        let names = search_dashboards(&client, &app.address, term).await;
        assert!(
            names.contains(&"Monthly Revenue".to_string()),
            "term {term:?} should match 'Monthly Revenue', got {names:?}"
        );
    }

    let by_tag = search_dashboards(&client, &app.address, "finance").await;
    assert!(
        by_tag.contains(&"Unrelated Dashboard Name".to_string()),
        "tag-field match failed, got {by_tag:?}"
    );

    let unfiltered = search_dashboards(&client, &app.address, "").await;
    assert!(
        unfiltered.len() >= 2,
        "empty q should return the unfiltered list, got {unfiltered:?}"
    );
}

/// Membership in the search result set must be decided by what the caller is actually served, not
/// by draft text they can't otherwise see. A non-editor
/// viewer must not be able to find a marker that exists only in an unpublished draft edit, while
/// the owner (who does see the draft) can; a marker that exists in the *published* text must still
/// be findable by the non-editor, so search still works for the common case.
#[tokio::test]
async fn query_search_cannot_read_unpublished_draft_text() {
    let app = skip_if_none!(spawn_app().await);
    app.cleanup().await;

    use arcodash_backend::services::query_service;
    use arcodash_core::test_support::new_data_source;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};

    let org_id = common::make_org(&app.db).await;
    let ds = new_data_source(org_id).insert(&app.db).await.unwrap();
    let (owner, _) = common::user_with_role(
        &app.db,
        org_id,
        &["view_query", "edit_query", "create_query"],
    )
    .await;
    let (viewer, _) = common::user_with_role(&app.db, org_id, &["view_query"]).await;

    let published_marker = format!("published-marker-{}", uuid::Uuid::new_v4().simple());
    let mut qa = arcodash_core::test_support::new_query(org_id, ds.id, owner.id);
    qa.api_key = Set(format!("search-oracle-{}", uuid::Uuid::new_v4().simple()));
    qa.query = Set(format!("SELECT 1 -- {published_marker}"));
    let q = qa.insert(&app.db).await.unwrap();
    let published = query_service::publish(&app.db, &owner, q.clone(), q.working_version)
        .await
        .expect("owner must be able to publish their own query");

    let by_published = query_service::list_for_user(
        &app.db,
        org_id,
        &viewer,
        false,
        Some(published_marker.clone()),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        by_published.iter().any(|x| x.id == q.id),
        "viewer must be able to find the query by its published text, got {by_published:?}"
    );

    // Owner edits the draft with a marker that is never published.
    let draft_marker = format!("draft-only-marker-{}", uuid::Uuid::new_v4().simple());
    query_service::update(
        &app.db,
        &owner,
        published,
        None,
        None,
        Some(format!("SELECT 2 -- {draft_marker}")),
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("owner must be able to edit their own draft");

    let owner_hits = query_service::list_for_user(
        &app.db,
        org_id,
        &owner,
        false,
        Some(draft_marker.clone()),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        owner_hits.iter().any(|x| x.id == q.id),
        "owner must see their own unpublished draft edit via search, got {owner_hits:?}"
    );

    let viewer_hits = query_service::list_for_user(
        &app.db,
        org_id,
        &viewer,
        false,
        Some(draft_marker.clone()),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        !viewer_hits.iter().any(|x| x.id == q.id),
        "a non-editor viewer must not find a draft-only marker via search: {viewer_hits:?}"
    );
}
