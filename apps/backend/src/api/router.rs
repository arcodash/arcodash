//! Mounts all `/api/*` routers, grouped by domain.

use std::sync::Arc;

use axum::routing::{delete, get, patch, post, put};
use axum::Router;

use crate::api::handlers::{
    ai_ask, ai_monitor_ask, ai_monitors, ai_query, ai_settings, ai_usage, ai_viz, ai_widget, auth,
    dashboard_narrative, dashboards, data_sources, destinations, embeds, events, favorites, groups,
    jobs, ldap, monitors, organization, permissions, queries, roles, runs, saml, schedules,
    snippets, status, users, visualizations, widgets,
};
use crate::middleware::rate_limit::{
    build_login_rate_limiter, keyed_ip_limiter, keyed_user_limiter, limit_by_ip, limit_by_user,
    quota_per_minute as per_minute, IpLimiterState, UserKeyedLimiter, UserLimiterState,
};
use crate::state::AppState;

fn ip_limited(routes: Router<AppState>, limiter: IpLimiterState) -> Router<AppState> {
    routes.route_layer(axum::middleware::from_fn_with_state(limiter, limit_by_ip))
}

fn user_limited(
    routes: Router<AppState>,
    state: &AppState,
    limiter: Arc<UserKeyedLimiter>,
) -> Router<AppState> {
    let state = UserLimiterState {
        app: state.clone(),
        limiter,
    };
    routes.route_layer(axum::middleware::from_fn_with_state(state, limit_by_user))
}

/// Build the `/api` router. Routes needing dedicated rate limits are mounted via `route_layer` on
/// small sub-routers, so e.g. the login budget never touches `/logout` or `/session`.
pub fn router(state: &AppState) -> Router<AppState> {
    // One shared budget for every pre-session credential route (login, SAML, LDAP);
    // separate limiters would give an attacker a budget per route against the same credential
    // store.
    let login_limiter = build_login_rate_limiter();

    let login_route = ip_limited(
        Router::new().route("/login", post(auth::login)),
        login_limiter.clone(),
    );

    // Per-user, generous: guards the target data source against runaway refresh loops, not
    // interactive use.
    let query_execute_route = user_limited(
        Router::new()
            .route("/query_results", post(queries::execute_ad_hoc))
            .route("/queries/{id}/results", post(queries::execute_saved))
            .route("/queries/{id}/refresh", post(queries::refresh))
            .route("/queries/{id}/run", post(queries::run))
            .route(
                "/queries/{id}/parameter_values",
                get(queries::parameter_values),
            ),
        state,
        keyed_user_limiter(per_minute("QUERY_EXECUTE_RATE_LIMIT_PER_MIN", 60)),
    );

    // Reading the cached latest-result pointer isn't a query execution; kept off the execute budget
    // and unlimited, in its own router (`route_layer` can't apply to a subset of a merged path).
    let query_latest_result_route =
        Router::new().route("/queries/{id}/results", get(queries::latest_result));

    // Per-user: CSV/XLSX rendering is real CPU work, so it gets its own budget.
    let result_download_limiter =
        keyed_user_limiter(per_minute("RESULT_DOWNLOAD_RATE_LIMIT_PER_MIN", 20));
    let result_download_route = user_limited(
        Router::new().route("/runs/{id}/result.csv", get(runs::result_csv)),
        state,
        result_download_limiter.clone(),
    );

    // `get_or_download_result` serves both the JSON read and downloads from one merged route (Axum
    // 0.8 can't split `{id}` / `{id}.{format}`), so the limiter is passed as an extension and
    // checked only on the download branch.
    let query_result_route = Router::new()
        .route("/query_results/{id}", get(queries::get_or_download_result))
        .layer(axum::Extension(result_download_limiter));

    // Per-user: connecting out to a configured host is the abusable part.
    let data_source_test_route = user_limited(
        Router::new().route("/data_sources/test", post(data_sources::test)),
        state,
        keyed_user_limiter(per_minute("DATA_SOURCE_TEST_RATE_LIMIT_PER_MIN", 10)),
    );

    // Per-user: an upload does real work (validation, object-store write).
    #[allow(
        clippy::cast_possible_truncation,
        reason = "u64 to usize is lossless on the 64-bit targets this server runs on"
    )]
    let data_source_upload_route = user_limited(
        Router::new()
            .route("/data_sources/uploads", post(data_sources::upload))
            .layer(axum::extract::DefaultBodyLimit::max(
                crate::services::upload_service::max_upload_bytes() as usize,
            )),
        state,
        keyed_user_limiter(per_minute("DATA_SOURCE_UPLOAD_RATE_LIMIT_PER_MIN", 10)),
    );

    let data_sources_route = Router::new()
        .route(
            "/data_sources",
            get(data_sources::list).post(data_sources::create),
        )
        .route(
            "/data_sources/{id}",
            get(data_sources::get)
                .post(data_sources::update)
                .delete(data_sources::delete),
        )
        .route("/data_sources/{id}/schema", get(data_sources::schema))
        .route(
            "/data_sources/{id}/ai_profile",
            get(data_sources::ai_profile),
        )
        .route(
            "/data_sources/{id}/ai_profile/rebuild",
            post(data_sources::rebuild_ai_profile),
        )
        .route("/data_sources/{id}/groups", get(data_sources::list_groups))
        .route("/data_sources/{id}/pause", post(data_sources::pause))
        .route("/data_sources/{id}/resume", post(data_sources::resume))
        .route(
            "/data_sources/{id}/suggest_monitors",
            post(ai_monitors::suggest_monitors),
        )
        .route("/data_sources/types", get(data_sources::types));

    let queries_route = Router::new()
        .route("/queries", get(queries::list).post(queries::create))
        // Static segments go before `{id}` so Axum doesn't treat them as an id.
        .route("/queries/favorites", get(favorites::list_favorite_queries))
        .route("/queries/tags", get(queries::tags))
        .route("/queries/generate_sql", post(ai_query::generate_sql))
        .route("/queries/ask", post(ai_ask::ask))
        .route(
            "/queries/{id}/suggest_monitors",
            post(ai_monitors::suggest_query_monitors),
        )
        .route(
            "/queries/suggest_visualization",
            post(ai_viz::suggest_visualization),
        )
        .route(
            "/queries/{id}",
            get(queries::get)
                .post(queries::update)
                .delete(queries::delete),
        )
        .route(
            "/queries/{id}/favorite",
            post(favorites::star_query).delete(favorites::unstar_query),
        )
        .route("/queries/{id}/version", get(queries::versions))
        .route(
            "/queries/{id}/visualizations",
            get(queries::list_visualizations),
        )
        .route("/queries/{id}/revert", post(queries::revert))
        .route("/queries/{id}/fork", post(queries::fork))
        .route("/queries/{id}/publish", post(queries::publish))
        .route("/queries/{id}/unpublish", post(queries::unpublish))
        .route("/queries/{id}/restore", post(queries::restore))
        .route("/queries/{id}/discard", post(queries::discard))
        .route("/queries/{id}/consumers", get(queries::consumers))
        // Axum 0.8 forbids two params per segment ({id}.{format}), so one handler splits on '.';
        // mounted via `query_result_route` above.
        .route("/jobs/{id}", get(jobs::get).delete(jobs::cancel))
        .route(
            "/queries/{id}/acl",
            get(permissions::list_query_acl).post(permissions::grant_query_acl),
        )
        .route(
            "/queries/{id}/acl/{perm_id}",
            delete(permissions::revoke_query_acl),
        )
        .route(
            "/queries/{id}/visibility",
            put(permissions::set_query_visibility),
        );

    let snippets_route = Router::new()
        .route(
            "/query_snippets",
            get(snippets::list).post(snippets::create),
        )
        .route(
            "/query_snippets/{id}",
            get(snippets::get)
                .post(snippets::update)
                .delete(snippets::delete),
        );

    let visualizations_route = Router::new()
        .route("/visualizations", post(visualizations::create))
        .route(
            "/visualizations/{id}",
            post(visualizations::update).delete(visualizations::delete),
        );

    let widgets_route = Router::new()
        .route("/widgets", post(widgets::create))
        .route(
            "/widgets/{id}",
            post(widgets::update).delete(widgets::delete),
        );

    // Anonymous, per-IP: a public token has no login to protect, so this is the only abuse control.
    let public_dashboard_route = ip_limited(
        Router::new().route("/dashboards/public/{token}", get(dashboards::public_get)),
        keyed_ip_limiter(per_minute("PUBLIC_DASHBOARD_RATE_LIMIT_PER_MIN", 30)),
    );

    // Anonymous, per-IP, same as the public dashboard route.
    let public_embed_route = ip_limited(
        Router::new().route("/embed/{token}", get(embeds::get_public_embed)),
        keyed_ip_limiter(per_minute("PUBLIC_EMBED_RATE_LIMIT_PER_MIN", 30)),
    );

    // Static segment (active_count) MUST be registered before /{id}.
    let embeds_route = Router::new()
        .route("/embeds", get(embeds::list).post(embeds::create))
        .route("/embeds/active_count", get(embeds::active_count))
        .route("/embeds/{id}", delete(embeds::revoke))
        .route(
            "/orgs/me/embed_settings",
            get(embeds::get_org_settings).post(embeds::set_org_settings),
        );

    // Static segments (tags, favorites) MUST be registered before /{slug}.
    let dashboards_route = Router::new()
        .route(
            "/dashboards",
            get(dashboards::list).post(dashboards::create),
        )
        .route("/dashboards/tags", get(dashboards::tags))
        .route("/dashboards/favorites", get(dashboards::favorites))
        .route(
            "/dashboards/{slug}",
            get(dashboards::get)
                .post(dashboards::update)
                .delete(dashboards::delete),
        )
        .route(
            "/dashboards/{slug}/share",
            post(dashboards::share)
                .put(dashboards::rotate_share)
                .delete(dashboards::revoke_share),
        )
        .route("/dashboards/{slug}/publish", post(dashboards::publish))
        .route("/dashboards/{slug}/unpublish", post(dashboards::unpublish))
        .route("/dashboards/{slug}/discard", post(dashboards::discard))
        .route("/dashboards/{slug}/layout", put(dashboards::save_layout))
        .route(
            "/dashboards/{id}/narrative",
            get(dashboard_narrative::latest_narrative).post(dashboard_narrative::explain_dashboard),
        )
        .route(
            "/dashboards/{id}/ai_widget",
            post(ai_widget::create_ai_widget),
        )
        .route(
            "/dashboards/{id}/favorite",
            post(dashboards::star).delete(dashboards::unstar),
        )
        .route(
            "/dashboards/{id}/acl",
            get(permissions::list_dashboard_acl).post(permissions::grant_dashboard_acl),
        )
        .route(
            "/dashboards/{id}/acl/{perm_id}",
            delete(permissions::revoke_dashboard_acl),
        )
        .route(
            "/dashboards/{id}/visibility",
            put(permissions::set_dashboard_visibility),
        );

    // Per-user, same profile as `/data_sources/test`.
    let destination_test_route = user_limited(
        Router::new().route("/destinations/test", post(destinations::test)),
        state,
        keyed_user_limiter(per_minute("DESTINATION_TEST_RATE_LIMIT_PER_MIN", 10)),
    );

    let destinations_route = Router::new()
        // Static path "types" MUST come before /{id}.
        .route("/destinations/types", get(destinations::types))
        .route(
            "/destinations",
            get(destinations::list).post(destinations::create),
        )
        .route(
            "/destinations/{id}",
            get(destinations::get)
                .post(destinations::update)
                .delete(destinations::delete),
        );

    // Per-user, generous: shares the `/queries/{id}/run` per-minute budget since it enqueues query
    // work.
    let monitor_run_route = user_limited(
        Router::new().route("/monitors/{id}/run", post(monitors::run)),
        state,
        keyed_user_limiter(per_minute("MONITOR_RUN_RATE_LIMIT_PER_MIN", 60)),
    );

    let monitors_route = Router::new()
        .route("/monitors", get(monitors::list).post(monitors::create))
        .route("/monitors/ask", post(ai_monitor_ask::ask_monitor))
        .route(
            "/monitors/{id}",
            get(monitors::get)
                .post(monitors::update)
                .delete(monitors::delete),
        )
        .route("/monitors/{id}/mute", post(monitors::mute))
        .route("/monitors/{id}/unmute", post(monitors::unmute))
        .route("/monitors/{id}/snooze", post(monitors::snooze))
        .route("/monitors/{id}/ack", post(monitors::ack))
        .route(
            "/monitors/{id}/escalation",
            get(monitors::get_escalation).put(monitors::put_escalation),
        )
        .route("/monitors/{id}/deliveries", get(monitors::list_deliveries))
        .route(
            "/monitors/{id}/observations",
            get(monitors::list_observations),
        )
        .route(
            "/monitors/{id}/subscriptions",
            get(monitors::list_subscriptions).post(monitors::subscribe),
        )
        .route(
            "/monitors/{id}/subscriptions/{sub_id}",
            delete(monitors::delete_subscription),
        )
        .route(
            "/monitor_subscriptions/{id}",
            delete(monitors::delete_subscription_flat),
        )
        .route(
            "/monitors/{id}/acl",
            get(permissions::list_monitor_acl).post(permissions::grant_monitor_acl),
        )
        .route(
            "/monitors/{id}/acl/{perm_id}",
            delete(permissions::revoke_monitor_acl),
        )
        .route(
            "/monitors/{id}/visibility",
            put(permissions::set_monitor_visibility),
        );

    // Per-user, conservative: each call sends a real notification to a third-party destination.
    let monitor_notify_route = user_limited(
        Router::new().route(
            "/monitors/{id}/subscriptions/{sub_id}/notify",
            post(monitors::notify_subscription),
        ),
        state,
        keyed_user_limiter(per_minute("MONITOR_NOTIFY_RATE_LIMIT_PER_MIN", 10)),
    );

    let schedules_route = Router::new()
        .route("/schedules", get(schedules::list).post(schedules::create))
        .route(
            "/schedules/{id}",
            patch(schedules::update).delete(schedules::delete),
        )
        .route("/runs", get(runs::list))
        .route("/runs/{id}", delete(runs::cancel));

    // Per-user, conservative: both routes send a real invitation email to a caller-chosen address.
    let user_invite_route = user_limited(
        Router::new().route("/users", post(users::invite)).route(
            "/users/{id}/resend_invitation",
            post(users::resend_invitation),
        ),
        state,
        keyed_user_limiter(per_minute("USER_INVITE_RATE_LIMIT_PER_MIN", 10)),
    );

    let users_route = Router::new()
        .route("/users", get(users::list))
        // Static segments before /{id}. `/users/invitation/verify` is mounted below under the
        // account-recovery limiter (unauthenticated enumeration surface).
        .route("/users/me/digest", post(users::set_digest))
        .route(
            "/users/{id}",
            get(users::get).post(users::update).delete(users::delete),
        )
        .route(
            "/users/{id}/regenerate_api_key",
            post(users::regenerate_api_key),
        )
        .route(
            "/users/{id}/effective_permissions",
            get(users::effective_permissions),
        )
        .route("/users/{id}/invitation", delete(users::revoke_invitation));

    // Anonymous, per-IP: a digest unsubscribe token has no login to protect.
    let digest_unsubscribe_route = ip_limited(
        Router::new().route("/digest/unsubscribe", get(users::unsubscribe_digest)),
        keyed_ip_limiter(per_minute("DIGEST_UNSUBSCRIBE_RATE_LIMIT_PER_MIN", 30)),
    );

    // Anonymous, per-IP, tight: pre-session recovery/invitation endpoints are
    // brute-force/enumeration targets.
    let account_recovery_route = ip_limited(
        Router::new()
            .route("/users/invitation/verify", get(users::verify_invitation))
            .route("/users/invitation/accept", post(users::accept_invitation))
            .route("/forgot_password", post(users::forgot_password))
            .route("/reset_password", post(users::reset_password)),
        keyed_ip_limiter(per_minute("ACCOUNT_RECOVERY_RATE_LIMIT_PER_MIN", 5)),
    );

    // Same per-IP login throttle as internal login: `/saml/acs` is unauthenticated and does
    // signature-verifying XML parsing.
    let saml_route = ip_limited(
        Router::new()
            .route("/saml/login", get(saml::login))
            .route("/saml/acs", post(saml::acs)),
        login_limiter.clone(),
    );

    // Same per-IP login throttle as internal login.
    let ldap_route = ip_limited(
        Router::new().route("/ldap/login", post(ldap::login)),
        login_limiter,
    );

    // Per-user, tight: bounds password probing (401 on mismatch) and Argon2 CPU cost.
    let change_password_route = user_limited(
        Router::new().route("/users/me/password", post(users::change_password)),
        state,
        keyed_user_limiter(per_minute("CHANGE_PASSWORD_RATE_LIMIT_PER_MIN", 5)),
    );

    // An `ExternalAuth` replaces the built-in login. Password recovery, invitation accept and
    // password change set or rotate a `password_hash`, so they are mounted only for built-in auth:
    // otherwise they would create a backdoor or be dead motion.
    let auth_routes = if state.external_auth.is_none() {
        Router::new()
            .merge(login_route)
            .merge(saml_route)
            .merge(ldap_route)
            .merge(account_recovery_route)
            .merge(change_password_route)
    } else {
        Router::new()
    };

    let groups_route = Router::new()
        .route("/groups", get(groups::list).post(groups::create))
        .route(
            "/groups/{id}",
            get(groups::get).post(groups::update).delete(groups::delete),
        )
        .route(
            "/groups/{id}/members",
            get(groups::list_members).post(groups::add_member),
        )
        .route(
            "/groups/{id}/members/{user_id}",
            delete(groups::remove_member),
        )
        .route(
            "/groups/{id}/data_sources",
            get(groups::list_data_sources).post(groups::assign_data_source),
        )
        .route(
            "/groups/{id}/data_sources/{ds_id}",
            delete(groups::remove_data_source),
        )
        .route(
            "/groups/{id}/data_source_access",
            put(groups::set_data_source_access),
        )
        .route(
            "/groups/{id}/roles",
            get(groups::list_roles).post(groups::add_role),
        )
        .route("/groups/{id}/roles/{role_id}", delete(groups::remove_role));

    let roles_route = Router::new()
        .route("/permissions", get(roles::list_permissions))
        .route("/roles", get(roles::list).post(roles::create))
        .route(
            "/roles/{id}",
            get(roles::get).post(roles::update).delete(roles::delete),
        );

    let admin_route = Router::new()
        .route(
            "/organization",
            get(organization::get).post(organization::update),
        )
        .route(
            "/organization/ai",
            get(ai_settings::get).post(ai_settings::update),
        )
        .route("/organization/ai/test", post(ai_settings::test_key))
        .route("/organization/ai/usage", get(ai_usage::get_usage))
        .route("/events", get(events::list))
        .route("/status", get(status::get));

    Router::new()
        // POST only: a GET could be triggered by cross-site navigation (SameSite=Lax still sends
        // the cookie), forcing a logout.
        .route("/logout", post(auth::logout))
        .route("/session", get(auth::session))
        .route("/health", get(status::health))
        .merge(auth_routes)
        .merge(users_route)
        .merge(user_invite_route)
        .merge(data_sources_route)
        .merge(data_source_test_route)
        .merge(data_source_upload_route)
        .merge(queries_route)
        .merge(query_execute_route)
        .merge(query_latest_result_route)
        .merge(query_result_route)
        .merge(result_download_route)
        .merge(snippets_route)
        .merge(visualizations_route)
        .merge(widgets_route)
        .merge(dashboards_route)
        .merge(public_dashboard_route)
        .merge(embeds_route)
        .merge(public_embed_route)
        .merge(digest_unsubscribe_route)
        .merge(destinations_route)
        .merge(destination_test_route)
        .merge(monitors_route)
        .merge(monitor_run_route)
        .merge(monitor_notify_route)
        .merge(schedules_route)
        .merge(groups_route)
        .merge(roles_route)
        .merge(admin_route)
}
