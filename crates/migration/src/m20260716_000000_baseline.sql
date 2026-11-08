-- Baseline schema for arcodash: the single source of truth for a fresh database.
-- All objects are schema-qualified so search_path is irrelevant.






-- Name: access_permissions; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.access_permissions (
    id bigint NOT NULL,
    object_type text NOT NULL,
    object_id bigint NOT NULL,
    access_type text NOT NULL,
    grantor_id bigint NOT NULL,
    grantee_id bigint,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    grantee_group_id bigint,
    CONSTRAINT ck_access_permissions_one_grantee CHECK (((grantee_id IS NOT NULL) <> (grantee_group_id IS NOT NULL)))
);


-- Name: access_permissions_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.access_permissions_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: access_permissions_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.access_permissions_id_seq OWNED BY public.access_permissions.id;


-- Name: api_keys; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.api_keys (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    object_type text NOT NULL,
    object_id bigint NOT NULL,
    api_key text NOT NULL,
    active boolean DEFAULT true NOT NULL,
    created_by_id bigint,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: api_keys_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.api_keys_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: api_keys_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.api_keys_id_seq OWNED BY public.api_keys.id;


-- Name: changes; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.changes (
    id bigint NOT NULL,
    object_type text NOT NULL,
    object_id bigint NOT NULL,
    user_id bigint NOT NULL,
    change jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: changes_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.changes_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: changes_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.changes_id_seq OWNED BY public.changes.id;


-- Name: conveyor_schedules; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.conveyor_schedules (
    id bigint NOT NULL,
    tenant_id bigint,
    task_type text NOT NULL,
    entity_id bigint,
    payload jsonb NOT NULL,
    spec jsonb NOT NULL,
    timezone text DEFAULT 'UTC'::text NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    paused_until timestamp with time zone,
    auto_paused_at timestamp with time zone,
    overlap_policy text DEFAULT 'skip'::text NOT NULL,
    next_run_at timestamp with time zone,
    consecutive_failures integer DEFAULT 0 NOT NULL,
    resource_key text,
    resource_cap bigint,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: conveyor_schedules_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.conveyor_schedules_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: conveyor_schedules_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.conveyor_schedules_id_seq OWNED BY public.conveyor_schedules.id;


-- Name: conveyor_tasks; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.conveyor_tasks (
    id bigint NOT NULL,
    tenant_id bigint,
    task_type text NOT NULL,
    entity_id bigint,
    schedule_id bigint,
    payload jsonb,
    resource_key text,
    resource_cap bigint,
    scheduled_for timestamp with time zone,
    triggered_by text NOT NULL,
    status text DEFAULT 'pending'::text NOT NULL,
    attempt integer DEFAULT 1 NOT NULL,
    waiting_on_task_id bigint,
    job_id text,
    worker_id text,
    lease_expires_at timestamp with time zone,
    enqueued_at timestamp with time zone,
    started_at timestamp with time zone,
    finished_at timestamp with time zone,
    duration_ms bigint,
    error text,
    result_ref jsonb,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: conveyor_tasks_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.conveyor_tasks_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: conveyor_tasks_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.conveyor_tasks_id_seq OWNED BY public.conveyor_tasks.id;


-- Name: dashboards; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.dashboards (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    slug text NOT NULL,
    name text NOT NULL,
    user_id bigint NOT NULL,
    layout jsonb DEFAULT '[]'::jsonb NOT NULL,
    dashboard_filters_enabled boolean DEFAULT false NOT NULL,
    is_archived boolean DEFAULT false NOT NULL,
    tags text[],
    options jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    search_vector tsvector,
    visibility text DEFAULT 'workspace'::text NOT NULL,
    published_snapshot jsonb
);


-- Name: dashboards_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.dashboards_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: dashboards_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.dashboards_id_seq OWNED BY public.dashboards.id;


-- Name: data_source_groups; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.data_source_groups (
    id bigint NOT NULL,
    data_source_id bigint NOT NULL,
    group_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: data_source_groups_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.data_source_groups_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: data_source_groups_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.data_source_groups_id_seq OWNED BY public.data_source_groups.id;


-- Name: data_sources; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.data_sources (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    name text NOT NULL,
    type text NOT NULL,
    encrypted_options bytea,
    scheduled_queue_name text,
    schema jsonb,
    schema_refreshed_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    paused_at timestamp with time zone,
    pause_reason text,
    max_concurrent_runs integer
);


-- Name: data_sources_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.data_sources_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: data_sources_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.data_sources_id_seq OWNED BY public.data_sources.id;


-- Name: embeds; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.embeds (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    object_type text NOT NULL,
    object_id bigint NOT NULL,
    mode text NOT NULL,
    token text,
    locked_params jsonb DEFAULT '{}'::jsonb NOT NULL,
    frame_ancestors jsonb,
    expires_at timestamp with time zone,
    created_by_id bigint,
    anon_authorized boolean DEFAULT false NOT NULL,
    revoked_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: embeds_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.embeds_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: embeds_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.embeds_id_seq OWNED BY public.embeds.id;


-- Name: escalation_steps; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.escalation_steps (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    monitor_id bigint NOT NULL,
    "position" integer NOT NULL,
    delay_minutes integer NOT NULL,
    destination_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: escalation_steps_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.escalation_steps_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: escalation_steps_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.escalation_steps_id_seq OWNED BY public.escalation_steps.id;


-- Name: events; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.events (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    user_id bigint,
    action text NOT NULL,
    object_type text,
    object_id bigint,
    additional_properties jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: events_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.events_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: events_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.events_id_seq OWNED BY public.events.id;


-- Name: favorites; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.favorites (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    object_type text NOT NULL,
    object_id bigint NOT NULL,
    user_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: favorites_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.favorites_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: favorites_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.favorites_id_seq OWNED BY public.favorites.id;


-- Name: group_roles; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.group_roles (
    id bigint NOT NULL,
    group_id bigint NOT NULL,
    role_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: group_roles_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.group_roles_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: group_roles_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.group_roles_id_seq OWNED BY public.group_roles.id;


-- Name: groups; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.groups (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    name text NOT NULL,
    permissions text[] NOT NULL,
    type text DEFAULT 'regular'::text NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    data_source_access_restricted boolean DEFAULT false NOT NULL
);


-- Name: groups_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.groups_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: groups_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.groups_id_seq OWNED BY public.groups.id;


-- Name: mail_deliveries; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.mail_deliveries (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    task_type text NOT NULL,
    entity_id bigint,
    to_email text,
    subject text,
    body_html text,
    status text DEFAULT 'pending'::text NOT NULL,
    error text,
    worker_id text,
    lease_expires_at timestamp with time zone,
    started_at timestamp with time zone,
    finished_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT mail_deliveries_task_type_check CHECK ((task_type = ANY (ARRAY['send_digest'::text, 'send_mail'::text])))
);


-- Name: mail_deliveries_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.mail_deliveries_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: mail_deliveries_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.mail_deliveries_id_seq OWNED BY public.mail_deliveries.id;


-- Name: monitor_subscriptions; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.monitor_subscriptions (
    id bigint NOT NULL,
    monitor_id bigint NOT NULL,
    user_id bigint NOT NULL,
    destination_id bigint,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: monitor_subscriptions_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.monitor_subscriptions_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: monitor_subscriptions_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.monitor_subscriptions_id_seq OWNED BY public.monitor_subscriptions.id;


-- Name: monitors; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.monitors (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    name text NOT NULL,
    query_id bigint NOT NULL,
    user_id bigint NOT NULL,
    options jsonb DEFAULT '{}'::jsonb NOT NULL,
    state text DEFAULT 'unknown'::text NOT NULL,
    last_triggered_at timestamp with time zone,
    rearm integer,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    acked_at timestamp with time zone,
    acked_by bigint,
    escalation_step integer DEFAULT 0 NOT NULL,
    last_escalated_at timestamp with time zone,
    visibility text DEFAULT 'workspace'::text NOT NULL
);


-- Name: monitors_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.monitors_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: monitors_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.monitors_id_seq OWNED BY public.monitors.id;


-- Name: notification_deliveries; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.notification_deliveries (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    monitor_id bigint NOT NULL,
    subscription_id bigint,
    destination_id bigint,
    state text NOT NULL,
    status text NOT NULL,
    error text,
    attempt_number integer DEFAULT 1 NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    run_id bigint,
    worker_id text,
    lease_expires_at timestamp with time zone,
    started_at timestamp with time zone,
    finished_at timestamp with time zone
);


-- Name: notification_deliveries_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.notification_deliveries_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: notification_deliveries_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.notification_deliveries_id_seq OWNED BY public.notification_deliveries.id;


-- Name: notification_destinations; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.notification_destinations (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    name text NOT NULL,
    type text NOT NULL,
    options bytea,
    user_id bigint,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: notification_destinations_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.notification_destinations_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: notification_destinations_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.notification_destinations_id_seq OWNED BY public.notification_destinations.id;


-- Name: org_embed_settings; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.org_embed_settings (
    org_id bigint NOT NULL,
    public_urls_enabled boolean DEFAULT true NOT NULL,
    embedding_secret bytea,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: organizations; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.organizations (
    id bigint NOT NULL,
    name text NOT NULL,
    slug text NOT NULL,
    settings jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: organizations_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.organizations_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: organizations_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.organizations_id_seq OWNED BY public.organizations.id;


-- Name: queries; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.queries (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    data_source_id bigint NOT NULL,
    name text NOT NULL,
    description text,
    query text NOT NULL,
    query_hash text NOT NULL,
    api_key text NOT NULL,
    user_id bigint NOT NULL,
    last_modified_by_id bigint,
    latest_query_data_id bigint,
    is_archived boolean DEFAULT false NOT NULL,
    options jsonb DEFAULT '{}'::jsonb NOT NULL,
    tags text[],
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    search_vector tsvector,
    last_execution_status text DEFAULT 'New'::text NOT NULL,
    last_execution_at timestamp with time zone,
    row_count integer,
    last_execution_error text,
    visibility text DEFAULT 'workspace'::text NOT NULL,
    published_name text,
    published_description text,
    published_query text,
    published_query_hash text,
    published_data_source_id bigint,
    published_options jsonb,
    published_latest_query_data_id bigint,
    published_change_id bigint,
    published_at timestamp with time zone,
    working_version integer DEFAULT 0 NOT NULL
);


-- Name: queries_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.queries_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: queries_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.queries_id_seq OWNED BY public.queries.id;


-- Name: query_results; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.query_results (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    data_source_id bigint NOT NULL,
    query_hash text NOT NULL,
    query text NOT NULL,
    data jsonb,
    data_location text,
    data_size_bytes bigint,
    runtime double precision,
    retrieved_at timestamp with time zone NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: query_results_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.query_results_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: query_results_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.query_results_id_seq OWNED BY public.query_results.id;


-- Name: query_snippets; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.query_snippets (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    trigger text NOT NULL,
    description text,
    snippet text NOT NULL,
    user_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: query_snippets_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.query_snippets_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: query_snippets_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.query_snippets_id_seq OWNED BY public.query_snippets.id;


-- Name: roles; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.roles (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    name text NOT NULL,
    permissions text[] NOT NULL,
    type text DEFAULT 'custom'::text NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: roles_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.roles_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: roles_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.roles_id_seq OWNED BY public.roles.id;


-- Name: runs; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.runs (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    entity_type text NOT NULL,
    entity_id bigint NOT NULL,
    schedule_id bigint,
    data_source_id bigint,
    scheduled_for timestamp with time zone,
    triggered_by text NOT NULL,
    status text DEFAULT 'pending'::text NOT NULL,
    waiting_on_run_id bigint,
    job_id text,
    worker_id text,
    lease_expires_at timestamp with time zone,
    started_at timestamp with time zone,
    finished_at timestamp with time zone,
    duration_ms bigint,
    row_count bigint,
    error text,
    data_age_secs bigint,
    query_result_id bigint,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    condition_met boolean,
    CONSTRAINT runs_entity_type_check CHECK ((entity_type = ANY (ARRAY['query'::text, 'monitor'::text]))),
    CONSTRAINT runs_status_check CHECK ((status = ANY (ARRAY['pending'::text, 'running'::text, 'success'::text, 'failed'::text, 'cancelled'::text, 'timed_out'::text, 'skipped'::text]))),
    CONSTRAINT runs_triggered_by_check CHECK ((triggered_by = ANY (ARRAY['schedule'::text, 'manual'::text, 'monitor'::text])))
);


-- Name: runs_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.runs_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: runs_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.runs_id_seq OWNED BY public.runs.id;


-- Name: schedules; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.schedules (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    entity_type text NOT NULL,
    entity_id bigint NOT NULL,
    spec jsonb NOT NULL,
    timezone text DEFAULT 'UTC'::text NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    paused_until timestamp with time zone,
    auto_paused_at timestamp with time zone,
    overlap_policy text DEFAULT 'skip'::text NOT NULL,
    next_run_at timestamp with time zone,
    consecutive_failures integer DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    CONSTRAINT schedules_entity_type_check CHECK ((entity_type = ANY (ARRAY['query'::text, 'monitor'::text]))),
    CONSTRAINT schedules_overlap_policy_check CHECK ((overlap_policy = ANY (ARRAY['skip'::text, 'coalesce'::text, 'parallel'::text, 'replace'::text])))
);


-- Name: schedules_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.schedules_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: schedules_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.schedules_id_seq OWNED BY public.schedules.id;


-- Name: users; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.users (
    id bigint NOT NULL,
    org_id bigint NOT NULL,
    name text NOT NULL,
    email text NOT NULL,
    password_hash text,
    groups bigint[] NOT NULL,
    api_key text NOT NULL,
    disabled_at timestamp with time zone,
    profile_image_url text,
    details jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    is_invitation_pending boolean DEFAULT false NOT NULL,
    digest_enabled boolean DEFAULT false NOT NULL,
    digest_unsubscribe_token text
);


-- Name: users_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.users_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: users_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.users_id_seq OWNED BY public.users.id;


-- Name: visualizations; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.visualizations (
    id bigint NOT NULL,
    query_id bigint NOT NULL,
    type text NOT NULL,
    name text NOT NULL,
    description text,
    options jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: visualizations_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.visualizations_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: visualizations_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.visualizations_id_seq OWNED BY public.visualizations.id;


-- Name: widgets; Type: TABLE; Schema: public; Owner: -

CREATE TABLE public.widgets (
    id bigint NOT NULL,
    dashboard_id bigint NOT NULL,
    visualization_id bigint,
    text text,
    width integer DEFAULT 1 NOT NULL,
    options jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL
);


-- Name: widgets_id_seq; Type: SEQUENCE; Schema: public; Owner: -

CREATE SEQUENCE public.widgets_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


-- Name: widgets_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -

ALTER SEQUENCE public.widgets_id_seq OWNED BY public.widgets.id;


-- Name: access_permissions id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.access_permissions ALTER COLUMN id SET DEFAULT nextval('public.access_permissions_id_seq'::regclass);


-- Name: api_keys id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.api_keys ALTER COLUMN id SET DEFAULT nextval('public.api_keys_id_seq'::regclass);


-- Name: changes id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.changes ALTER COLUMN id SET DEFAULT nextval('public.changes_id_seq'::regclass);


-- Name: conveyor_schedules id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.conveyor_schedules ALTER COLUMN id SET DEFAULT nextval('public.conveyor_schedules_id_seq'::regclass);


-- Name: conveyor_tasks id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.conveyor_tasks ALTER COLUMN id SET DEFAULT nextval('public.conveyor_tasks_id_seq'::regclass);


-- Name: dashboards id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.dashboards ALTER COLUMN id SET DEFAULT nextval('public.dashboards_id_seq'::regclass);


-- Name: data_source_groups id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.data_source_groups ALTER COLUMN id SET DEFAULT nextval('public.data_source_groups_id_seq'::regclass);


-- Name: data_sources id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.data_sources ALTER COLUMN id SET DEFAULT nextval('public.data_sources_id_seq'::regclass);


-- Name: embeds id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.embeds ALTER COLUMN id SET DEFAULT nextval('public.embeds_id_seq'::regclass);


-- Name: escalation_steps id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.escalation_steps ALTER COLUMN id SET DEFAULT nextval('public.escalation_steps_id_seq'::regclass);


-- Name: events id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.events ALTER COLUMN id SET DEFAULT nextval('public.events_id_seq'::regclass);


-- Name: favorites id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.favorites ALTER COLUMN id SET DEFAULT nextval('public.favorites_id_seq'::regclass);


-- Name: group_roles id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.group_roles ALTER COLUMN id SET DEFAULT nextval('public.group_roles_id_seq'::regclass);


-- Name: groups id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.groups ALTER COLUMN id SET DEFAULT nextval('public.groups_id_seq'::regclass);


-- Name: mail_deliveries id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.mail_deliveries ALTER COLUMN id SET DEFAULT nextval('public.mail_deliveries_id_seq'::regclass);


-- Name: monitor_subscriptions id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.monitor_subscriptions ALTER COLUMN id SET DEFAULT nextval('public.monitor_subscriptions_id_seq'::regclass);


-- Name: monitors id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.monitors ALTER COLUMN id SET DEFAULT nextval('public.monitors_id_seq'::regclass);


-- Name: notification_deliveries id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.notification_deliveries ALTER COLUMN id SET DEFAULT nextval('public.notification_deliveries_id_seq'::regclass);


-- Name: notification_destinations id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.notification_destinations ALTER COLUMN id SET DEFAULT nextval('public.notification_destinations_id_seq'::regclass);


-- Name: organizations id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.organizations ALTER COLUMN id SET DEFAULT nextval('public.organizations_id_seq'::regclass);


-- Name: queries id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.queries ALTER COLUMN id SET DEFAULT nextval('public.queries_id_seq'::regclass);


-- Name: query_results id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.query_results ALTER COLUMN id SET DEFAULT nextval('public.query_results_id_seq'::regclass);


-- Name: query_snippets id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.query_snippets ALTER COLUMN id SET DEFAULT nextval('public.query_snippets_id_seq'::regclass);


-- Name: roles id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.roles ALTER COLUMN id SET DEFAULT nextval('public.roles_id_seq'::regclass);


-- Name: runs id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.runs ALTER COLUMN id SET DEFAULT nextval('public.runs_id_seq'::regclass);


-- Name: schedules id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.schedules ALTER COLUMN id SET DEFAULT nextval('public.schedules_id_seq'::regclass);


-- Name: users id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.users ALTER COLUMN id SET DEFAULT nextval('public.users_id_seq'::regclass);


-- Name: visualizations id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.visualizations ALTER COLUMN id SET DEFAULT nextval('public.visualizations_id_seq'::regclass);


-- Name: widgets id; Type: DEFAULT; Schema: public; Owner: -

ALTER TABLE ONLY public.widgets ALTER COLUMN id SET DEFAULT nextval('public.widgets_id_seq'::regclass);


-- Name: access_permissions access_permissions_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.access_permissions
    ADD CONSTRAINT access_permissions_pkey PRIMARY KEY (id);


-- Name: api_keys api_keys_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.api_keys
    ADD CONSTRAINT api_keys_pkey PRIMARY KEY (id);


-- Name: changes changes_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.changes
    ADD CONSTRAINT changes_pkey PRIMARY KEY (id);


-- Name: conveyor_schedules conveyor_schedules_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.conveyor_schedules
    ADD CONSTRAINT conveyor_schedules_pkey PRIMARY KEY (id);


-- Name: conveyor_tasks conveyor_tasks_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.conveyor_tasks
    ADD CONSTRAINT conveyor_tasks_pkey PRIMARY KEY (id);


-- Name: dashboards dashboards_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.dashboards
    ADD CONSTRAINT dashboards_pkey PRIMARY KEY (id);


-- Name: data_source_groups data_source_groups_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.data_source_groups
    ADD CONSTRAINT data_source_groups_pkey PRIMARY KEY (id);


-- Name: data_sources data_sources_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.data_sources
    ADD CONSTRAINT data_sources_pkey PRIMARY KEY (id);


-- Name: embeds embeds_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.embeds
    ADD CONSTRAINT embeds_pkey PRIMARY KEY (id);


-- Name: escalation_steps escalation_steps_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.escalation_steps
    ADD CONSTRAINT escalation_steps_pkey PRIMARY KEY (id);


-- Name: events events_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.events
    ADD CONSTRAINT events_pkey PRIMARY KEY (id);


-- Name: favorites favorites_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.favorites
    ADD CONSTRAINT favorites_pkey PRIMARY KEY (id);


-- Name: group_roles group_roles_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.group_roles
    ADD CONSTRAINT group_roles_pkey PRIMARY KEY (id);


-- Name: groups groups_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.groups
    ADD CONSTRAINT groups_pkey PRIMARY KEY (id);


-- Name: mail_deliveries mail_deliveries_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.mail_deliveries
    ADD CONSTRAINT mail_deliveries_pkey PRIMARY KEY (id);


-- Name: monitor_subscriptions monitor_subscriptions_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.monitor_subscriptions
    ADD CONSTRAINT monitor_subscriptions_pkey PRIMARY KEY (id);


-- Name: monitors monitors_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.monitors
    ADD CONSTRAINT monitors_pkey PRIMARY KEY (id);


-- Name: notification_deliveries notification_deliveries_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.notification_deliveries
    ADD CONSTRAINT notification_deliveries_pkey PRIMARY KEY (id);


-- Name: notification_destinations notification_destinations_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.notification_destinations
    ADD CONSTRAINT notification_destinations_pkey PRIMARY KEY (id);


-- Name: org_embed_settings org_embed_settings_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.org_embed_settings
    ADD CONSTRAINT org_embed_settings_pkey PRIMARY KEY (org_id);


-- Name: organizations organizations_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.organizations
    ADD CONSTRAINT organizations_pkey PRIMARY KEY (id);


-- Name: queries queries_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.queries
    ADD CONSTRAINT queries_pkey PRIMARY KEY (id);


-- Name: query_results query_results_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.query_results
    ADD CONSTRAINT query_results_pkey PRIMARY KEY (id);


-- Name: query_snippets query_snippets_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.query_snippets
    ADD CONSTRAINT query_snippets_pkey PRIMARY KEY (id);


-- Name: roles roles_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.roles
    ADD CONSTRAINT roles_pkey PRIMARY KEY (id);


-- Name: runs runs_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.runs
    ADD CONSTRAINT runs_pkey PRIMARY KEY (id);


-- Name: schedules schedules_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.schedules
    ADD CONSTRAINT schedules_pkey PRIMARY KEY (id);


-- Name: users users_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);


-- Name: visualizations visualizations_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.visualizations
    ADD CONSTRAINT visualizations_pkey PRIMARY KEY (id);


-- Name: widgets widgets_pkey; Type: CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.widgets
    ADD CONSTRAINT widgets_pkey PRIMARY KEY (id);


-- Name: idx_access_permissions_object; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_access_permissions_object ON public.access_permissions USING btree (object_type, object_id);


-- Name: idx_access_permissions_unique_group; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_access_permissions_unique_group ON public.access_permissions USING btree (object_type, object_id, grantee_group_id) WHERE (grantee_group_id IS NOT NULL);


-- Name: idx_access_permissions_unique_user; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_access_permissions_unique_user ON public.access_permissions USING btree (object_type, object_id, grantee_id) WHERE (grantee_id IS NOT NULL);


-- Name: idx_api_keys_api_key; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_api_keys_api_key ON public.api_keys USING btree (api_key);


-- Name: idx_changes_object; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_changes_object ON public.changes USING btree (object_type, object_id);


-- Name: idx_conveyor_schedules_deadline; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_conveyor_schedules_deadline ON public.conveyor_schedules USING btree (next_run_at) WHERE ((enabled = true) AND (auto_paused_at IS NULL));


-- Name: idx_conveyor_tasks_live; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_conveyor_tasks_live ON public.conveyor_tasks USING btree (status, lease_expires_at) WHERE (status = ANY (ARRAY['pending'::text, 'running'::text]));


-- Name: idx_conveyor_tasks_waiting; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_conveyor_tasks_waiting ON public.conveyor_tasks USING btree (waiting_on_task_id) WHERE (waiting_on_task_id IS NOT NULL);


-- Name: idx_dashboards_org_id_slug; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_dashboards_org_id_slug ON public.dashboards USING btree (org_id, slug);


-- Name: idx_dashboards_search_vector; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_dashboards_search_vector ON public.dashboards USING gin (search_vector);


-- Name: idx_dashboards_tags; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_dashboards_tags ON public.dashboards USING gin (tags);


-- Name: idx_data_source_groups_ds_group; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_data_source_groups_ds_group ON public.data_source_groups USING btree (data_source_id, group_id);


-- Name: idx_embeds_object; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_embeds_object ON public.embeds USING btree (object_type, object_id);


-- Name: idx_embeds_org_id; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_embeds_org_id ON public.embeds USING btree (org_id);


-- Name: idx_embeds_token_unique; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_embeds_token_unique ON public.embeds USING btree (token) WHERE (token IS NOT NULL);


-- Name: idx_escalation_steps_monitor_id_position; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_escalation_steps_monitor_id_position ON public.escalation_steps USING btree (monitor_id, "position");


-- Name: idx_events_org_id_created_at; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_events_org_id_created_at ON public.events USING btree (org_id, created_at);


-- Name: idx_favorites_user_object; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_favorites_user_object ON public.favorites USING btree (user_id, object_type, object_id);


-- Name: idx_group_roles_group_role; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_group_roles_group_role ON public.group_roles USING btree (group_id, role_id);


-- Name: idx_groups_org_id_name; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_groups_org_id_name ON public.groups USING btree (org_id, name);


-- Name: idx_mail_deliveries_stale_pending; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_mail_deliveries_stale_pending ON public.mail_deliveries USING btree (created_at) WHERE (status = 'pending'::text);


-- Name: idx_monitor_subscriptions_unique_null_dest; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_monitor_subscriptions_unique_null_dest ON public.monitor_subscriptions USING btree (monitor_id, user_id) WHERE (destination_id IS NULL);


-- Name: idx_monitor_subscriptions_unique_with_dest; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_monitor_subscriptions_unique_with_dest ON public.monitor_subscriptions USING btree (monitor_id, user_id, destination_id) WHERE (destination_id IS NOT NULL);


-- Name: idx_notification_deliveries_monitor_id_created_at; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_notification_deliveries_monitor_id_created_at ON public.notification_deliveries USING btree (monitor_id, created_at DESC);


-- Name: idx_notification_deliveries_run; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_notification_deliveries_run ON public.notification_deliveries USING btree (run_id) WHERE (run_id IS NOT NULL);


-- Name: idx_notification_deliveries_stale_pending; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_notification_deliveries_stale_pending ON public.notification_deliveries USING btree (created_at) WHERE (status = 'pending'::text);


-- Name: idx_organizations_slug; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_organizations_slug ON public.organizations USING btree (slug);


-- Name: idx_queries_api_key; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_queries_api_key ON public.queries USING btree (api_key);


-- Name: idx_queries_org_id_not_archived; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_queries_org_id_not_archived ON public.queries USING btree (org_id) WHERE (NOT is_archived);


-- Name: idx_queries_search_vector; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_queries_search_vector ON public.queries USING gin (search_vector);


-- Name: idx_queries_tags; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_queries_tags ON public.queries USING gin (tags);


-- Name: idx_query_results_org_ds_hash; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_query_results_org_ds_hash ON public.query_results USING btree (org_id, data_source_id, query_hash);


-- Name: idx_roles_org_id_name; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_roles_org_id_name ON public.roles USING btree (org_id, name);


-- Name: idx_runs_ds_running; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_runs_ds_running ON public.runs USING btree (data_source_id) WHERE (status = 'running'::text);


-- Name: idx_runs_entity_hist; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_runs_entity_hist ON public.runs USING btree (entity_type, entity_id, id DESC);


-- Name: idx_runs_live; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_runs_live ON public.runs USING btree (status, lease_expires_at) WHERE (status = ANY (ARRAY['pending'::text, 'running'::text]));


-- Name: idx_runs_waiting; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_runs_waiting ON public.runs USING btree (waiting_on_run_id) WHERE (waiting_on_run_id IS NOT NULL);


-- Name: idx_schedules_deadline; Type: INDEX; Schema: public; Owner: -

CREATE INDEX idx_schedules_deadline ON public.schedules USING btree (next_run_at) WHERE ((enabled = true) AND (auto_paused_at IS NULL));


-- Name: idx_users_api_key; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_users_api_key ON public.users USING btree (api_key);


-- Name: idx_users_digest_unsubscribe_token; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_users_digest_unsubscribe_token ON public.users USING btree (digest_unsubscribe_token);


-- Name: idx_users_org_id_email; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX idx_users_org_id_email ON public.users USING btree (org_id, email);


-- Name: uq_conveyor_schedules_entity; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX uq_conveyor_schedules_entity ON public.conveyor_schedules USING btree (task_type, entity_id);


-- Name: uq_conveyor_tasks_entity_tick; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX uq_conveyor_tasks_entity_tick ON public.conveyor_tasks USING btree (task_type, entity_id, scheduled_for) WHERE (schedule_id IS NOT NULL);


-- Name: uq_conveyor_tasks_inflight; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX uq_conveyor_tasks_inflight ON public.conveyor_tasks USING btree (task_type, entity_id) WHERE (status = ANY (ARRAY['pending'::text, 'running'::text]));


-- Name: uq_runs_entity_tick; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX uq_runs_entity_tick ON public.runs USING btree (entity_type, entity_id, scheduled_for) WHERE (schedule_id IS NOT NULL);


-- Name: uq_runs_inflight; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX uq_runs_inflight ON public.runs USING btree (entity_type, entity_id) WHERE (status = ANY (ARRAY['pending'::text, 'running'::text]));


-- Name: uq_schedules_entity_type_entity_id; Type: INDEX; Schema: public; Owner: -

CREATE UNIQUE INDEX uq_schedules_entity_type_entity_id ON public.schedules USING btree (entity_type, entity_id);


-- Name: access_permissions fk_access_permissions_grantee_group_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.access_permissions
    ADD CONSTRAINT fk_access_permissions_grantee_group_id FOREIGN KEY (grantee_group_id) REFERENCES public.groups(id) ON DELETE CASCADE;


-- Name: access_permissions fk_access_permissions_grantee_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.access_permissions
    ADD CONSTRAINT fk_access_permissions_grantee_id FOREIGN KEY (grantee_id) REFERENCES public.users(id) ON DELETE CASCADE;


-- Name: access_permissions fk_access_permissions_grantor_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.access_permissions
    ADD CONSTRAINT fk_access_permissions_grantor_id FOREIGN KEY (grantor_id) REFERENCES public.users(id) ON DELETE CASCADE;


-- Name: api_keys fk_api_keys_created_by_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.api_keys
    ADD CONSTRAINT fk_api_keys_created_by_id FOREIGN KEY (created_by_id) REFERENCES public.users(id) ON DELETE SET NULL;


-- Name: api_keys fk_api_keys_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.api_keys
    ADD CONSTRAINT fk_api_keys_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: changes fk_changes_user_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.changes
    ADD CONSTRAINT fk_changes_user_id FOREIGN KEY (user_id) REFERENCES public.users(id) DEFERRABLE;


-- Name: dashboards fk_dashboards_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.dashboards
    ADD CONSTRAINT fk_dashboards_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: dashboards fk_dashboards_user_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.dashboards
    ADD CONSTRAINT fk_dashboards_user_id FOREIGN KEY (user_id) REFERENCES public.users(id) DEFERRABLE;


-- Name: data_source_groups fk_data_source_groups_data_source_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.data_source_groups
    ADD CONSTRAINT fk_data_source_groups_data_source_id FOREIGN KEY (data_source_id) REFERENCES public.data_sources(id) ON DELETE CASCADE;


-- Name: data_source_groups fk_data_source_groups_group_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.data_source_groups
    ADD CONSTRAINT fk_data_source_groups_group_id FOREIGN KEY (group_id) REFERENCES public.groups(id) ON DELETE CASCADE;


-- Name: data_sources fk_data_sources_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.data_sources
    ADD CONSTRAINT fk_data_sources_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: embeds fk_embeds_created_by_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.embeds
    ADD CONSTRAINT fk_embeds_created_by_id FOREIGN KEY (created_by_id) REFERENCES public.users(id) ON DELETE SET NULL;


-- Name: embeds fk_embeds_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.embeds
    ADD CONSTRAINT fk_embeds_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: escalation_steps fk_escalation_steps_destination_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.escalation_steps
    ADD CONSTRAINT fk_escalation_steps_destination_id FOREIGN KEY (destination_id) REFERENCES public.notification_destinations(id) ON DELETE CASCADE;


-- Name: escalation_steps fk_escalation_steps_monitor_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.escalation_steps
    ADD CONSTRAINT fk_escalation_steps_monitor_id FOREIGN KEY (monitor_id) REFERENCES public.monitors(id) ON DELETE CASCADE;


-- Name: escalation_steps fk_escalation_steps_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.escalation_steps
    ADD CONSTRAINT fk_escalation_steps_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: events fk_events_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.events
    ADD CONSTRAINT fk_events_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: events fk_events_user_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.events
    ADD CONSTRAINT fk_events_user_id FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE SET NULL;


-- Name: favorites fk_favorites_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.favorites
    ADD CONSTRAINT fk_favorites_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: favorites fk_favorites_user_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.favorites
    ADD CONSTRAINT fk_favorites_user_id FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


-- Name: group_roles fk_group_roles_group_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.group_roles
    ADD CONSTRAINT fk_group_roles_group_id FOREIGN KEY (group_id) REFERENCES public.groups(id) ON DELETE CASCADE;


-- Name: group_roles fk_group_roles_role_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.group_roles
    ADD CONSTRAINT fk_group_roles_role_id FOREIGN KEY (role_id) REFERENCES public.roles(id) ON DELETE CASCADE;


-- Name: groups fk_groups_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.groups
    ADD CONSTRAINT fk_groups_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: mail_deliveries fk_mail_deliveries_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.mail_deliveries
    ADD CONSTRAINT fk_mail_deliveries_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: monitor_subscriptions fk_monitor_subscriptions_destination_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.monitor_subscriptions
    ADD CONSTRAINT fk_monitor_subscriptions_destination_id FOREIGN KEY (destination_id) REFERENCES public.notification_destinations(id) ON DELETE CASCADE;


-- Name: monitor_subscriptions fk_monitor_subscriptions_monitor_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.monitor_subscriptions
    ADD CONSTRAINT fk_monitor_subscriptions_monitor_id FOREIGN KEY (monitor_id) REFERENCES public.monitors(id) ON DELETE CASCADE;


-- Name: monitor_subscriptions fk_monitor_subscriptions_user_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.monitor_subscriptions
    ADD CONSTRAINT fk_monitor_subscriptions_user_id FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


-- Name: monitors fk_monitors_acked_by; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.monitors
    ADD CONSTRAINT fk_monitors_acked_by FOREIGN KEY (acked_by) REFERENCES public.users(id) ON DELETE SET NULL;


-- Name: monitors fk_monitors_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.monitors
    ADD CONSTRAINT fk_monitors_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: monitors fk_monitors_query_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.monitors
    ADD CONSTRAINT fk_monitors_query_id FOREIGN KEY (query_id) REFERENCES public.queries(id) ON DELETE CASCADE;


-- Name: monitors fk_monitors_user_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.monitors
    ADD CONSTRAINT fk_monitors_user_id FOREIGN KEY (user_id) REFERENCES public.users(id) DEFERRABLE;


-- Name: notification_deliveries fk_notification_deliveries_monitor_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.notification_deliveries
    ADD CONSTRAINT fk_notification_deliveries_monitor_id FOREIGN KEY (monitor_id) REFERENCES public.monitors(id) ON DELETE CASCADE;


-- Name: notification_deliveries fk_notification_deliveries_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.notification_deliveries
    ADD CONSTRAINT fk_notification_deliveries_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: notification_deliveries fk_notification_deliveries_subscription_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.notification_deliveries
    ADD CONSTRAINT fk_notification_deliveries_subscription_id FOREIGN KEY (subscription_id) REFERENCES public.monitor_subscriptions(id) ON DELETE SET NULL;


-- Name: notification_destinations fk_notification_destinations_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.notification_destinations
    ADD CONSTRAINT fk_notification_destinations_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: notification_destinations fk_notification_destinations_user_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.notification_destinations
    ADD CONSTRAINT fk_notification_destinations_user_id FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE SET NULL;


-- Name: org_embed_settings fk_org_embed_settings_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.org_embed_settings
    ADD CONSTRAINT fk_org_embed_settings_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: queries fk_queries_data_source_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.queries
    ADD CONSTRAINT fk_queries_data_source_id FOREIGN KEY (data_source_id) REFERENCES public.data_sources(id) DEFERRABLE;


-- Name: queries fk_queries_last_modified_by_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.queries
    ADD CONSTRAINT fk_queries_last_modified_by_id FOREIGN KEY (last_modified_by_id) REFERENCES public.users(id) ON DELETE SET NULL;


-- Name: queries fk_queries_latest_query_data_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.queries
    ADD CONSTRAINT fk_queries_latest_query_data_id FOREIGN KEY (latest_query_data_id) REFERENCES public.query_results(id) ON DELETE SET NULL;


-- Name: queries fk_queries_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.queries
    ADD CONSTRAINT fk_queries_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: queries fk_queries_published_change_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.queries
    ADD CONSTRAINT fk_queries_published_change_id FOREIGN KEY (published_change_id) REFERENCES public.changes(id) ON DELETE SET NULL;


-- Name: queries fk_queries_published_data_source_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.queries
    ADD CONSTRAINT fk_queries_published_data_source_id FOREIGN KEY (published_data_source_id) REFERENCES public.data_sources(id);


-- Name: queries fk_queries_published_latest_query_data_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.queries
    ADD CONSTRAINT fk_queries_published_latest_query_data_id FOREIGN KEY (published_latest_query_data_id) REFERENCES public.query_results(id) ON DELETE SET NULL;


-- Name: queries fk_queries_user_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.queries
    ADD CONSTRAINT fk_queries_user_id FOREIGN KEY (user_id) REFERENCES public.users(id) DEFERRABLE;


-- Name: query_results fk_query_results_data_source_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.query_results
    ADD CONSTRAINT fk_query_results_data_source_id FOREIGN KEY (data_source_id) REFERENCES public.data_sources(id) ON DELETE CASCADE;


-- Name: query_results fk_query_results_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.query_results
    ADD CONSTRAINT fk_query_results_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: query_snippets fk_query_snippets_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.query_snippets
    ADD CONSTRAINT fk_query_snippets_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: query_snippets fk_query_snippets_user_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.query_snippets
    ADD CONSTRAINT fk_query_snippets_user_id FOREIGN KEY (user_id) REFERENCES public.users(id) DEFERRABLE;


-- Name: roles fk_roles_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.roles
    ADD CONSTRAINT fk_roles_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: runs fk_runs_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.runs
    ADD CONSTRAINT fk_runs_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: runs fk_runs_query_result_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.runs
    ADD CONSTRAINT fk_runs_query_result_id FOREIGN KEY (query_result_id) REFERENCES public.query_results(id) ON DELETE SET NULL;


-- Name: runs fk_runs_schedule_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.runs
    ADD CONSTRAINT fk_runs_schedule_id FOREIGN KEY (schedule_id) REFERENCES public.schedules(id) ON DELETE SET NULL;


-- Name: schedules fk_schedules_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.schedules
    ADD CONSTRAINT fk_schedules_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: users fk_users_org_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.users
    ADD CONSTRAINT fk_users_org_id FOREIGN KEY (org_id) REFERENCES public.organizations(id) ON DELETE CASCADE;


-- Name: visualizations fk_visualizations_query_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.visualizations
    ADD CONSTRAINT fk_visualizations_query_id FOREIGN KEY (query_id) REFERENCES public.queries(id) ON DELETE CASCADE;


-- Name: widgets fk_widgets_dashboard_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.widgets
    ADD CONSTRAINT fk_widgets_dashboard_id FOREIGN KEY (dashboard_id) REFERENCES public.dashboards(id) ON DELETE CASCADE;


-- Name: widgets fk_widgets_visualization_id; Type: FK CONSTRAINT; Schema: public; Owner: -

ALTER TABLE ONLY public.widgets
    ADD CONSTRAINT fk_widgets_visualization_id FOREIGN KEY (visualization_id) REFERENCES public.visualizations(id) ON DELETE CASCADE;




