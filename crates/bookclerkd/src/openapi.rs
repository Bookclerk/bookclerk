//! The daemon's OpenAPI document, served at `GET /api/openapi.json`.
//!
//! Every route the daemon documents is registered through a utoipa-axum
//! [`OpenApiRouter`] from its handler's `#[utoipa::path]`, so a documented
//! route and its spec entry cannot drift apart. The route groups carry no
//! state or auth layers ([`RouteGroup`]): [`crate::api::router`]
//! layers them for serving, and [`document`] reads their paths, adding each
//! group's [`Access`] as the operation's security requirements.
//!
//! Routes registered with a plain `.route(…)` (the SPA documents, the
//! static assets, the invite page) are left out of the document on purpose;
//! the `only_listed_routes_bypass_the_spec` test keeps that list closed.
//!
//! The document is OpenAPI 3.1. [`v30`] restates it as OpenAPI 3.0.3 for
//! tools that only read 3.0, served at [`v30::OPENAPI_V30_PATH`].

pub mod v30;

use std::sync::Arc;

use axum::Json;
use utoipa::openapi::path::{Operation, PathItem};
use utoipa::openapi::security::{
    ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme,
};
use utoipa::openapi::tag::Tag;
use utoipa::openapi::{InfoBuilder, LicenseBuilder, OpenApi, OpenApiBuilder};
use utoipa_axum::router::OpenApiRouter;

use crate::api::AppState;
use crate::auth::{PORTAL_SESSION_COOKIE, SESSION_COOKIE};

/// Path the document is served at.
pub const OPENAPI_PATH: &str = "/api/openapi.json";

/// Schema of an error response declared without a body: an object with at
/// least an `error` slug.
const API_ERROR: &str = "ApiError";

/// Security scheme: the operator token as `Authorization: Bearer <token>`.
const OPERATOR_TOKEN: &str = "operatorToken";

/// Security scheme: the operator's browser session cookie.
const OPERATOR_SESSION: &str = "operatorSession";

/// Security scheme: a library user's (portal) browser session cookie.
const PORTAL_SESSION: &str = "portalSession";

/// Security scheme: a node's signed `Authorization: Bookclerk-Node …` header.
const NODE_SIGNATURE: &str = "nodeSignature";

/// Security scheme: an upload grant's `Authorization: Bookclerk-Upload …` header.
const UPLOAD_TOKEN: &str = "uploadToken";

/// Security scheme: an access token this daemon issued as an OIDC provider.
const OIDC_ACCESS_TOKEN: &str = "oidcAccessToken";

/// Who a route group admits, as enforced by its auth layer.
///
/// The variants mirror the `require_*` middleware in [`crate::auth`]; the
/// document states each as security requirements plus an `x-bookclerk-access`
/// extension with the role rule in words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// No auth layer. The handler checks what it needs (a session, a
    /// signed request, a form secret) and says so in its description.
    Public,
    /// `require_operator_auth`: the operator token or session only.
    Operator,
    /// `require_operator_or_administrator_auth`: the operator, or a user
    /// with the Administrator role or above.
    OperatorOrAdministrator,
    /// `require_operator_or_owner_auth`: the operator, or an Owner.
    OperatorOrOwner,
    /// `require_operator_or_node_owner_auth`: the operator, or a user who
    /// owns at least one node.
    OperatorOrNodeOwner,
    /// `require_operator_or_user_auth`: the operator, or any signed-in user.
    OperatorOrUser,
    /// `require_operator_or_portal_auth`: the operator, or any portal session.
    OperatorOrPortal,
}

impl Access {
    /// True when the layer resolves a library user's session and so refuses
    /// a user who still has to enroll a second factor
    /// ([`crate::totp::enrollment_blocks`]) before the handler runs: every
    /// layer but the public and operator-only ones.
    fn gates_enrollment(self) -> bool {
        match self {
            Access::Public | Access::Operator => false,
            Access::OperatorOrAdministrator
            | Access::OperatorOrOwner
            | Access::OperatorOrNodeOwner
            | Access::OperatorOrUser
            | Access::OperatorOrPortal => true,
        }
    }

    /// The security schemes any one of which satisfies the layer.
    ///
    /// # Returns
    ///
    /// Scheme names from [`security_schemes`]; empty for [`Access::Public`].
    fn schemes(self) -> &'static [&'static str] {
        match self {
            Access::Public => &[],
            Access::Operator => &[OPERATOR_TOKEN, OPERATOR_SESSION],
            Access::OperatorOrAdministrator
            | Access::OperatorOrOwner
            | Access::OperatorOrNodeOwner
            | Access::OperatorOrUser
            | Access::OperatorOrPortal => &[OPERATOR_TOKEN, OPERATOR_SESSION, PORTAL_SESSION],
        }
    }

    /// The role rule in words, for the `x-bookclerk-access` extension.
    ///
    /// # Returns
    ///
    /// One sentence naming who may call the route.
    fn rule(self) -> &'static str {
        match self {
            Access::Public => {
                "No auth layer. Routes that need a caller say so in `security`; the handler checks it."
            }
            Access::Operator => "The operator (token or session) only.",
            Access::OperatorOrAdministrator => {
                "The operator, or a signed-in Administrator, Owner or user allowed to acquire."
            }
            Access::OperatorOrOwner => "The operator, or a signed-in Owner.",
            Access::OperatorOrNodeOwner => {
                "The operator, a signed-in Owner, Administrator or Host, or a user who owns at least one node."
            }
            Access::OperatorOrUser => "The operator, or any signed-in user.",
            Access::OperatorOrPortal => "The operator, or any library session.",
        }
    }
}

/// The security schemes the document declares.
///
/// # Returns
///
/// (name, scheme) for the operator Bearer token, the two session cookies, and
/// the credentials single routes check themselves: node signatures, upload
/// tokens and OIDC access tokens.
fn security_schemes() -> [(&'static str, SecurityScheme); 6] {
    [
        (
            OPERATOR_TOKEN,
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "The operator token (`bookclerk daemon token`), as `Authorization: Bearer <token>`.",
                    ))
                    .build(),
            ),
        ),
        (
            OPERATOR_SESSION,
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                SESSION_COOKIE,
                "The operator's browser session, set by `POST /api/auth/login`.",
            ))),
        ),
        (
            PORTAL_SESSION,
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                PORTAL_SESSION_COOKIE,
                "A library user's browser session, set by a sign-in route. Cookie-authenticated writes must send an `Origin` (or `Referer`) matching the daemon's public origin or its host.",
            ))),
        ),
        (
            NODE_SIGNATURE,
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "Authorization",
                "A node's signed request, as `Authorization: Bookclerk-Node <node_id> <issued_at> <signature>` with the node's identity key.",
            ))),
        ),
        (
            UPLOAD_TOKEN,
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "Authorization",
                "The token of an upload grant, as `Authorization: Bookclerk-Upload <token>`.",
            ))),
        ),
        (
            OIDC_ACCESS_TOKEN,
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "An access token from this daemon's OIDC token endpoint (`POST /oidc/token`), as `Authorization: Bearer <token>`.",
                    ))
                    .build(),
            ),
        ),
    ]
}

/// Tags, in document order, and the path prefix that selects each.
///
/// A path takes the tag of the longest matching prefix; [`document`] fails a
/// debug assertion for a documented path no prefix matches.
const TAGS: &[(&str, &str, &str)] = &[
    ("Health", "/health", "Liveness."),
    ("Spec", OPENAPI_PATH, "This document."),
    ("Spec", v30::OPENAPI_V30_PATH, "This document."),
    ("Auth", "/api/auth", "Sign-in, sessions, passkeys, TOTP and the operator's elevation."),
    ("Users", "/api/users", "Library users and their claim tickets."),
    ("Users", "/api/avatars", "Library users and their claim tickets."),
    ("Library", "/api/library", "Books in the library, scans and acquires."),
    ("Discover", "/api/discover", "Recommendations, catalog search and title metadata."),
    ("Discover", "/api/covers", "Recommendations, catalog search and title metadata."),
    ("Discover", "/api/stores", "Recommendations, catalog search and title metadata."),
    ("Wishlist", "/api/wishlist", "Titles users have asked for."),
    ("Wishlist", "/api/request-queue", "Titles users have asked for."),
    ("Jobs", "/api/jobs", "Queued and running control-plane jobs."),
    ("Events", "/api/events", "Domain events and their deliveries."),
    ("Events", "/api/queues", "Domain events and their deliveries."),
    ("Live", "/api/live", "The web interface's live-updates socket."),
    ("Status", "/api/status", "Library and daemon snapshot."),
    ("Status", "/api/cluster", "Library and daemon snapshot."),
    ("Settings", "/api/settings", "Daemon settings and user preferences."),
    ("Settings", "/api/preferences", "Daemon settings and user preferences."),
    ("Settings", "/api/database", "Daemon settings and user preferences."),
    ("Plugins", "/api/plugins", "Plugin packages, instances, consent and deployment."),
    ("Plugins", "/api/config/plugins", "Plugin packages, instances, consent and deployment."),
    ("Plugins", "/api/config/plugin-instances", "Plugin packages, instances, consent and deployment."),
    ("Configuration", "/api/config", "Live configuration domains and reloads."),
    ("Cluster", "/api/config/nodes", "Nodes, clerks, branches, joining a library and host enrollment."),
    ("Cluster", "/api/config/clerks", "Nodes, clerks, branches, joining a library and host enrollment."),
    ("Cluster", "/api/config/node-invites", "Nodes, clerks, branches, joining a library and host enrollment."),
    ("Cluster", "/api/config/node-credentials", "Nodes, clerks, branches, joining a library and host enrollment."),
    ("Cluster", "/api/config/host-enrollment", "Nodes, clerks, branches, joining a library and host enrollment."),
    ("Cluster", "/api/config/branches", "Nodes, clerks, branches, joining a library and host enrollment."),
    ("Cluster", "/api/config/join", "Nodes, clerks, branches, joining a library and host enrollment."),
    ("Cluster", "/api/config/cluster-consents", "Nodes, clerks, branches, joining a library and host enrollment."),
    ("Cluster", "/api/config/clusters", "Nodes, clerks, branches, joining a library and host enrollment."),
    ("Accounts", "/api/portal", "Store accounts a library user connects (the Accounts page)."),
    ("Node protocol", "/api/node/v1", "Node-to-node enrollment, socket, artifacts and uploads; each route checks its own credential."),
    ("OIDC provider", "/.well-known", "Bookclerk as an OpenID Connect provider for other apps."),
    ("OIDC provider", "/oidc", "Bookclerk as an OpenID Connect provider for other apps."),
    ("OIDC provider", "/api/auth/oidc/clients", "Bookclerk as an OpenID Connect provider for other apps."),
    ("Legacy", "/status", "Old unprefixed paths; use the `/api/…` route named in each."),
    ("Legacy", "/scan", "Old unprefixed paths; use the `/api/…` route named in each."),
    ("Legacy", "/acquire", "Old unprefixed paths; use the `/api/…` route named in each."),
    ("Legacy", "/jobs", "Old unprefixed paths; use the `/api/…` route named in each."),
    ("Legacy", "/integrations", "Old unprefixed paths; use the `/api/…` route named in each."),
];

/// Picks the tag for a path: the longest [`TAGS`] prefix that matches whole
/// segments.
///
/// # Arguments
///
/// * `path` - A documented path template (`/api/users/{id}`).
///
/// # Returns
///
/// The tag name, or `None` when no prefix matches.
fn tag_for(path: &str) -> Option<&'static str> {
    TAGS.iter()
        .filter(|(_, prefix, _)| {
            path == *prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('.'))
        })
        .max_by_key(|(_, prefix, _)| prefix.len())
        .map(|(tag, _, _)| *tag)
}

/// Every operation of a path item with its HTTP method, mutably, for a
/// pass that treats safe and unsafe methods differently.
///
/// # Arguments
///
/// * `item` - One path's operations.
///
/// # Returns
///
/// The operations that are set, each with its method.
fn operations_with_methods_mut(item: &mut PathItem) -> Vec<(axum::http::Method, &mut Operation)> {
    use axum::http::Method;
    [
        (Method::GET, item.get.as_mut()),
        (Method::PUT, item.put.as_mut()),
        (Method::POST, item.post.as_mut()),
        (Method::DELETE, item.delete.as_mut()),
        (Method::OPTIONS, item.options.as_mut()),
        (Method::HEAD, item.head.as_mut()),
        (Method::PATCH, item.patch.as_mut()),
        (Method::TRACE, item.trace.as_mut()),
    ]
    .into_iter()
    .filter_map(|(method, op)| op.map(|op| (method, op)))
    .collect()
}

/// Every operation of a path item, mutably.
///
/// # Arguments
///
/// * `item` - One path's operations.
///
/// # Returns
///
/// The operations that are set.
fn operations_mut(item: &mut PathItem) -> Vec<&mut Operation> {
    [
        item.get.as_mut(),
        item.put.as_mut(),
        item.post.as_mut(),
        item.delete.as_mut(),
        item.options.as_mut(),
        item.head.as_mut(),
        item.patch.as_mut(),
        item.trace.as_mut(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Takes a route group's paths into the document, stating its access.
///
/// # Arguments
///
/// * `doc` - The document being built.
/// * `group` - The group's paths and schemas.
/// * `access` - Who the group's auth layer admits.
/// * `prefix` - Where the group is nested (`""` when merged at the root).
fn add_group(doc: &mut OpenApi, mut group: OpenApi, access: Access, prefix: &str) {
    use utoipa::openapi::ResponseBuilder;
    for (path, item) in group.paths.paths.iter_mut() {
        let full_path = format!("{prefix}{path}");
        for (method, op) in operations_with_methods_mut(item) {
            // The auth layer answers before the handler runs: 401 without a
            // credential, and 403 when the caller's role falls short (every
            // layer but the any-session one checks a role).
            let refusals: &[(&str, &str)] = match access {
                Access::Public => &[],
                Access::OperatorOrPortal => &[(
                    "401",
                    "The caller is not signed in, or the credential is wrong.",
                )],
                Access::Operator
                | Access::OperatorOrAdministrator
                | Access::OperatorOrOwner
                | Access::OperatorOrNodeOwner
                | Access::OperatorOrUser => &[
                    (
                        "401",
                        "The caller is not signed in, or the credential is wrong.",
                    ),
                    ("403", "The caller's role does not allow this route."),
                ],
            };
            for (status, description) in refusals {
                op.responses
                    .responses
                    .entry((*status).to_string())
                    .or_insert_with(|| {
                        ResponseBuilder::new()
                            .description(*description)
                            .build()
                            .into()
                    });
            }
            if op.security.is_none() {
                op.security = Some(
                    access
                        .schemes()
                        .iter()
                        .map(|scheme| SecurityRequirement::new(*scheme, Vec::<String>::new()))
                        .collect(),
                );
            }
            if access.gates_enrollment() && !crate::totp::enrollment_exempt(&full_path) {
                // The any-session layer checks no role, so a safe method
                // there has no generic `forbidden`; a write still has the
                // CSRF refusal.
                let generic_forbidden = access != Access::OperatorOrPortal || !method.is_safe();
                add_enrollment_refusal(op, generic_forbidden);
            }
        }
    }
    // The one operator-only route whose layer admits an impersonating
    // session (it ends the impersonation) and checks no role, so the
    // group's 403 never happens there (`require_operator_auth`).
    if let Some(op) = group
        .paths
        .paths
        .get_mut("/api/auth/impersonate")
        .and_then(|item| item.delete.as_mut())
    {
        op.responses.responses.remove("403");
    }
    for item in group.paths.paths.values_mut() {
        for op in operations_mut(item) {
            let extensions = op.extensions.get_or_insert_with(Default::default);
            extensions.insert("x-bookclerk-access".to_string(), access.rule().into());
        }
    }
    if prefix.is_empty() {
        doc.merge(group);
    } else {
        *doc = std::mem::take(doc).nest(prefix, group);
    }
}

/// A route group the daemon serves under the root, before state and auth.
pub(crate) struct RouteGroup {
    /// The group's routes, each registered from its `#[utoipa::path]`.
    pub routes: OpenApiRouter<Arc<AppState>>,
    /// Who the group's auth layer admits.
    pub access: Access,
}

impl RouteGroup {
    /// Pairs routes with their access.
    ///
    /// # Arguments
    ///
    /// * `routes` - The group's routes.
    /// * `access` - Who may call them.
    ///
    /// # Returns
    ///
    /// The group.
    pub fn new(routes: OpenApiRouter<Arc<AppState>>, access: Access) -> Self {
        Self { routes, access }
    }
}

/// A route group as the document reads it.
pub(crate) struct DocumentedGroup {
    /// The group's paths and schemas.
    pub openapi: OpenApi,
    /// Who the group's auth layer admits.
    pub access: Access,
    /// Where the group is nested (`""` when merged at the root).
    pub prefix: &'static str,
}

impl DocumentedGroup {
    /// Reads a group's paths.
    ///
    /// # Arguments
    ///
    /// * `router` - The group, without state or layers.
    /// * `access` - Who its auth layer admits.
    ///
    /// # Returns
    ///
    /// The group, merged at the root.
    pub fn new<S: Clone + Send + Sync + 'static>(router: OpenApiRouter<S>, access: Access) -> Self {
        Self {
            openapi: router.into_openapi(),
            access,
            prefix: "",
        }
    }

    /// The same group nested under `prefix`.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Path the group is nested at (`/api/portal`).
    ///
    /// # Returns
    ///
    /// The group.
    pub fn nested(self, prefix: &'static str) -> Self {
        Self { prefix, ..self }
    }
}

/// Builds the daemon's OpenAPI document.
///
/// # Returns
///
/// The document: every route group from [`crate::api::documented_groups`],
/// with security, tags and server-independent paths.
pub fn document() -> OpenApi {
    let mut doc = OpenApiBuilder::new()
        .info(
            InfoBuilder::new()
                .title("Bookclerk daemon API")
                .version(env!("CARGO_PKG_VERSION"))
                .description(Some(
                    "The HTTP API of `bookclerkd`, the Bookclerk daemon: the web interface, the CLI and \
                     other nodes use it. Request bodies are JSON (`Content-Type: application/json`; \
                     send `{}` for defaults) unless a route says otherwise. Error responses are JSON \
                     objects with an `error` slug (`ApiError`); bare statuses get \
                     `{\"error\", \"message\", \"status\"}` (`ErrorJson`).",
                ))
                .license(Some(LicenseBuilder::new().name(env!("CARGO_PKG_LICENSE")).build()))
                .build(),
        )
        .build();
    for group in crate::api::documented_groups() {
        add_group(&mut doc, group.openapi, group.access, group.prefix);
    }
    let components = doc.components.get_or_insert_with(Default::default);
    for (name, scheme) in security_schemes() {
        components.add_security_scheme(name, scheme);
    }
    let mut used = Vec::new();
    for (path, item) in doc.paths.paths.iter_mut() {
        let tag = tag_for(path);
        debug_assert!(tag.is_some(), "no OpenAPI tag for {path}");
        let Some(tag) = tag else { continue };
        for op in operations_mut(item) {
            op.tags = Some(vec![tag.to_string()]);
        }
        if !used.contains(&tag) {
            used.push(tag);
        }
    }
    let mut tags: Vec<Tag> = Vec::new();
    for (name, _, description) in TAGS {
        if used.contains(name) && !tags.iter().any(|t| t.name == *name) {
            let mut tag = Tag::new(*name);
            tag.description = Some((*description).to_string());
            tags.push(tag);
        }
    }
    doc.tags = Some(tags);
    doc.components
        .get_or_insert_with(Default::default)
        .schemas
        .insert(API_ERROR.to_string(), api_error_schema());
    add_middleware_refusals(&mut doc);
    fill_untyped_content(&mut doc);
    add_extractor_rejections(&mut doc);
    add_middleware_error_examples(&mut doc);
    close_nested_output_schemas(&mut doc);
    doc
}

/// Gives every error response the middleware answers its example: the
/// `ErrorJson` [`crate::http_error::brand_error_responses`] writes for that
/// status (slug, message and status, as [`crate::http_error::ErrorJson::for_status`]
/// builds them) on a JSON body typed [`API_ERROR`] or `ErrorJson`, the
/// 504's text on its `text/plain` body, and the branded page
/// ([`crate::http_error::branded_page`]) on a `text/html` body. A response whose route states its
/// own example (`example` or `examples`) keeps it, so a handler that writes
/// its own words shows them.
///
/// # Arguments
///
/// * `doc` - The document, after [`fill_untyped_content`] typed every error body.
fn add_middleware_error_examples(doc: &mut OpenApi) {
    use axum::http::StatusCode;
    use utoipa::openapi::schema::Schema;
    use utoipa::openapi::RefOr;
    let branded = |schema: &Option<RefOr<Schema>>| match schema {
        Some(RefOr::Ref(reference)) => {
            let name = reference
                .ref_location
                .rsplit('/')
                .next()
                .unwrap_or_default();
            name == API_ERROR || name == "ErrorJson"
        }
        _ => false,
    };
    for item in doc.paths.paths.values_mut() {
        for op in operations_mut(item) {
            for (status, response) in op.responses.responses.iter_mut() {
                let RefOr::T(response) = response else {
                    continue;
                };
                let Some(status) = status
                    .parse()
                    .ok()
                    .and_then(|code| StatusCode::from_u16(code).ok())
                else {
                    continue;
                };
                if !(status.is_client_error() || status.is_server_error()) {
                    continue;
                }
                for (media, content) in response.content.iter_mut() {
                    let RefOr::T(content) = content else { continue };
                    if content.example.is_some() || !content.examples.is_empty() {
                        continue;
                    }
                    if media.starts_with("application/json") && branded(&content.schema) {
                        content.example =
                            serde_json::to_value(crate::http_error::ErrorJson::for_status(status))
                                .ok();
                    } else if media == "text/plain" && status == StatusCode::GATEWAY_TIMEOUT {
                        content.example = Some(crate::api::API_TIMEOUT_MESSAGE.into());
                    } else if media == "text/html" {
                        content.example = Some(crate::http_error::branded_page(status).into());
                    }
                }
            }
        }
    }
}

/// Schemas nested in `OutputSettingsV1` whose unknown keys the daemon
/// rejects: its deserializer checks `lame` and each `replacement_characters`
/// rule itself (`reject_unknown_keys` in `bookclerk_library::control_plane::output`),
/// while the same `bookclerk_config` types parsed from `config.toml` keep
/// ignoring unknown keys, so the `additionalProperties: false` the published
/// contract needs cannot come from a serde attribute on the types.
const CLOSED_OUTPUT_SCHEMAS: &[&str] = &["LameConfig", "ReplacementRule"];

/// Publishes `additionalProperties: false` on [`CLOSED_OUTPUT_SCHEMAS`], so
/// a client validating against the document rejects what the route would.
///
/// # Arguments
///
/// * `doc` - The document, after every group is added.
fn close_nested_output_schemas(doc: &mut OpenApi) {
    use utoipa::openapi::schema::{AdditionalProperties, Schema};
    use utoipa::openapi::RefOr;
    let Some(components) = doc.components.as_mut() else {
        return;
    };
    for name in CLOSED_OUTPUT_SCHEMAS {
        if let Some(RefOr::T(Schema::Object(object))) = components.schemas.get_mut(*name) {
            object.additional_properties = Some(Box::new(AdditionalProperties::FreeForm(false)));
        }
    }
}

/// The [`API_ERROR`] schema: `error` always, `message` and `status` when the
/// error middleware wrote the body, and whatever else a handler adds.
///
/// # Returns
///
/// The schema.
fn api_error_schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
    use utoipa::openapi::schema::{ObjectBuilder, Type};
    ObjectBuilder::new()
        .schema_type(Type::Object)
        .description(Some(
            "An error. `error` is a stable slug; bodies the error middleware writes also carry `message` and `status`, and some routes add fields of their own.",
        ))
        .property(
            "error",
            ObjectBuilder::new()
                .schema_type(Type::String)
                .description(Some("Stable machine slug."))
                .examples([serde_json::json!("not_found")]),
        )
        .required("error")
        .property(
            "message",
            ObjectBuilder::new()
                .schema_type(Type::String)
                .description(Some("Explanation for people."))
                .examples([serde_json::json!("That page or resource was not found.")]),
        )
        .property(
            "status",
            ObjectBuilder::new()
                .schema_type(Type::Integer)
                .description(Some("The HTTP status, repeated."))
                .examples([serde_json::json!(404)]),
        )
        .build()
        .into()
}

/// Routes whose handler reads its JSON body itself (to verify a signature
/// over the raw bytes, or to answer a malformed body with its own words),
/// so axum's `Json` extractor never rejects a request for them and their
/// `#[utoipa::path]` states the outcomes the handler writes.
const MANUAL_JSON_BODIES: &[&str] = &[
    "/api/node/v1/enroll",
    "/api/node/v1/enroll/oauth",
    "/api/auth/branch-consent",
    "/api/node/v1/uploads/{upload_id}/commit",
];

/// The three refusals axum's `Json` extractor writes before a handler
/// runs, as `(status, description, how the route's description is
/// extended when it already states the status)`.
const EXTRACTOR_REJECTIONS: [(&str, &str, &str); 3] = [
    (
        "400",
        "The body is not syntactically valid JSON.",
        "As `text/plain`, when the body is not syntactically valid JSON.",
    ),
    (
        "415",
        "The request did not declare a JSON body.",
        "As `text/plain`, when the request did not declare a JSON body.",
    ),
    (
        "422",
        "The JSON does not fit the body: a missing field, a wrong type or an unknown shape.",
        "As `text/plain`, when the JSON does not fit the body.",
    ),
];

/// A request body's `application/json` schema resolved to the object serde
/// deserializes: the schema's name when it is a component (`$ref`), and
/// the object itself.
struct BodyObject<'a> {
    /// The component name (the Rust type's), or `None` for an inline body.
    name: Option<&'a str>,
    /// The object schema.
    object: &'a utoipa::openapi::schema::Object,
}

/// The text axum's `Json` extractor answers with for each of
/// [`EXTRACTOR_REJECTIONS`]: the 400 and 415 are the extractor's fixed
/// messages; the 422 is the message serde writes for `{}` when the body has
/// a required field, for a wrong type in its first string, boolean or array
/// field, or for `null` when every field is optional and none of those
/// kinds (the derived deserializer names the struct).
///
/// # Arguments
///
/// * `status` - One of `400`, `415` or `422`.
/// * `body` - The request body's schema, resolved to its object form.
///
/// # Returns
///
/// The response text.
fn extractor_rejection_text(status: &str, body: Option<&BodyObject<'_>>) -> String {
    use utoipa::openapi::schema::{Schema, SchemaType, Type};
    use utoipa::openapi::RefOr;
    const DATA: &str = "Failed to deserialize the JSON body into the target type";
    match status {
        "400" => {
            "Failed to parse the request body as JSON: expected value at line 1 column 1".into()
        }
        "415" => "Expected request with `Content-Type: application/json`".into(),
        _ => {
            let Some(BodyObject { name, object: body }) = body else {
                return format!("{DATA}: invalid type: null, expected a map at line 1 column 4");
            };
            if let Some(field) = body.required.first() {
                return format!("{DATA}: missing field `{field}` at line 1 column 2");
            }
            let typed = |want: Type| {
                body.properties.iter().find_map(|(name, schema)| {
                    let schema_type = match schema {
                        RefOr::T(Schema::Object(object)) => &object.schema_type,
                        RefOr::T(Schema::Array(array)) => &array.schema_type,
                        RefOr::T(_) | RefOr::Ref(_) => return None,
                    };
                    let kinds: Vec<Type> = match schema_type {
                        SchemaType::Type(kind) => vec![kind.clone()],
                        SchemaType::Array(kinds) => kinds.clone(),
                        SchemaType::AnyValue => Vec::new(),
                    };
                    kinds.contains(&want).then_some(name.as_str())
                })
            };
            // serde reports the column after the offending `1` in `{"<field>":1}`.
            let wrong = |field: &str, expected: &str| {
                let column = field.len() + 5;
                format!("{DATA}: {field}: invalid type: integer `1`, expected {expected} at line 1 column {column}")
            };
            if let Some(field) = typed(Type::String) {
                return wrong(field, "a string");
            }
            if let Some(field) = typed(Type::Boolean) {
                return wrong(field, "a boolean");
            }
            if let Some(field) = typed(Type::Array) {
                return wrong(field, "a sequence");
            }
            match name {
                Some(name) => {
                    format!("{DATA}: invalid type: null, expected struct {name} at line 1 column 4")
                }
                None => format!("{DATA}: invalid type: null, expected a map at line 1 column 4"),
            }
        }
    }
}

/// Resolves a request body's `application/json` schema to the object serde
/// deserializes: a `$ref` is looked up in `components`, and the
/// `oneOf [T, null]` of an optional body yields `T`.
///
/// # Arguments
///
/// * `schema` - The media type's schema.
/// * `components` - The document's components, for `$ref`s.
///
/// # Returns
///
/// The object schema with its component name, or `None` when the body is
/// not an object.
fn body_object<'a>(
    schema: &'a utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
    components: &'a utoipa::openapi::Components,
) -> Option<BodyObject<'a>> {
    use utoipa::openapi::schema::Schema;
    use utoipa::openapi::RefOr;
    match schema {
        RefOr::Ref(reference) => {
            let name = reference.ref_location.rsplit('/').next()?;
            let mut body = body_object(components.schemas.get(name)?, components)?;
            body.name.get_or_insert(name);
            Some(body)
        }
        RefOr::T(Schema::Object(object)) => Some(BodyObject { name: None, object }),
        RefOr::T(Schema::OneOf(one_of)) => one_of
            .items
            .iter()
            .find_map(|item| body_object(item, components)),
        RefOr::T(_) => None,
    }
}

/// The extractor's message prefixes, by which a route's own `text/plain`
/// example is told from one that already documents the extractor.
const EXTRACTOR_PREFIXES: [&str; 3] = [
    "Failed to parse the request body as JSON",
    "Expected request with `Content-Type: application/json`",
    "Failed to deserialize the JSON body into the target type",
];

/// Adds the extractor's text to a `text/plain` response the route already
/// states: the route's own example moves to the named `route` example and
/// the extractor's becomes `extractor`, so both stay documented. A response
/// whose example is already the extractor's is left as it is.
///
/// # Arguments
///
/// * `content` - The route's `text/plain` media type.
/// * `text` - The extractor's text for this status.
///
/// # Returns
///
/// Whether the content changed (the route's own text was kept beside the
/// extractor's), so the caller extends the description.
fn add_extractor_example(content: &mut utoipa::openapi::Content, text: &str) -> bool {
    use utoipa::openapi::example::ExampleBuilder;
    use utoipa::openapi::RefOr;
    let documents_extractor = |value: &serde_json::Value| {
        value
            .as_str()
            .is_some_and(|value| EXTRACTOR_PREFIXES.iter().any(|p| value.starts_with(p)))
    };
    if content.examples.values().any(|example| match example {
        RefOr::T(example) => example.value.as_ref().is_some_and(documents_extractor),
        RefOr::Ref(_) => false,
    }) || content.examples.contains_key("extractor")
    {
        return false;
    }
    if let Some(own) = content.example.take() {
        if documents_extractor(&own) {
            content.example = Some(own);
            return false;
        }
        content.examples.insert(
            "route".to_string(),
            RefOr::T(
                ExampleBuilder::new()
                    .summary("The route's own refusal")
                    .value(Some(own))
                    .build(),
            ),
        );
    }
    content.examples.insert(
        "extractor".to_string(),
        RefOr::T(
            ExampleBuilder::new()
                .summary("The JSON extractor's refusal")
                .value(Some(text.into()))
                .build(),
        ),
    );
    true
}

/// States the refusals axum's `Json` extractor writes for every route that
/// reads a JSON body, as the `text/plain` responses it sends: 400 for a
/// body that is not JSON, 415 for a request without
/// `Content-Type: application/json` (not on a route whose body is optional,
/// where the extractor yields no body instead) and 422 for JSON that does
/// not fit. A route that already states a status keeps its own body and
/// gets the `text/plain` one beside it; a route whose own `text/plain`
/// text is not the extractor's keeps it as a named example beside the
/// extractor's; [`MANUAL_JSON_BODIES`] are skipped.
///
/// # Arguments
///
/// * `doc` - The document, after [`fill_untyped_content`] gave every error
///   response declared without a body its JSON one, so a route's bare 400
///   keeps that body beside the extractor's text.
fn add_extractor_rejections(doc: &mut OpenApi) {
    use utoipa::openapi::schema::{ObjectBuilder, Type};
    use utoipa::openapi::{ContentBuilder, RefOr, Required, ResponseBuilder};
    let components = doc.components.clone().unwrap_or_default();
    for (path, item) in doc.paths.paths.iter_mut() {
        if MANUAL_JSON_BODIES.contains(&path.as_str()) {
            continue;
        }
        for op in operations_mut(item) {
            let Some(RefOr::T(body)) = op.request_body.as_ref() else {
                continue;
            };
            let Some(RefOr::T(json)) = body
                .content
                .iter()
                .find(|(media, _)| media.starts_with("application/json"))
                .map(|(_, content)| content)
            else {
                continue;
            };
            let optional = body.required != Some(Required::True);
            let object = json
                .schema
                .as_ref()
                .and_then(|schema| body_object(schema, &components));
            for (status, description, extension) in EXTRACTOR_REJECTIONS {
                if status == "415" && optional {
                    continue;
                }
                let text = extractor_rejection_text(status, object.as_ref());
                let content = ContentBuilder::new()
                    .schema(Some(ObjectBuilder::new().schema_type(Type::String)))
                    .example(Some(text.clone().into()))
                    .build();
                match op.responses.responses.get_mut(status) {
                    Some(RefOr::T(response)) => {
                        match response.content.get_mut("text/plain") {
                            Some(RefOr::T(existing)) => {
                                if !add_extractor_example(existing, &text) {
                                    continue;
                                }
                            }
                            Some(RefOr::Ref(_)) => continue,
                            None => {
                                response
                                    .content
                                    .insert("text/plain".to_string(), RefOr::T(content));
                            }
                        }
                        response.description = format!("{} {extension}", response.description);
                    }
                    Some(RefOr::Ref(_)) => {}
                    None => {
                        op.responses.responses.insert(
                            status.to_string(),
                            ResponseBuilder::new()
                                .description(description)
                                .content("text/plain", content)
                                .build()
                                .into(),
                        );
                    }
                }
            }
        }
    }
}

/// States the outcomes the router's outer layers produce before a handler
/// answers: the 504 [`crate::api::api_timeout_middleware`] returns when an
/// `/api/` request runs past its budget (every route but the streamed
/// upload `PUT`), and the 403 [`crate::csrf::require_csrf_for_cookie_api`]
/// returns for a cookie-carrying `/api/` write whose `Origin` or `Referer`
/// does not match (every write but the exempt sign-in routes, and only where
/// a session cookie can be the credential).
///
/// # Arguments
///
/// * `doc` - The document, after every group is added.
fn add_middleware_refusals(doc: &mut OpenApi) {
    use axum::http::Method;
    use utoipa::openapi::schema::{ObjectBuilder, Type};
    use utoipa::openapi::{ContentBuilder, RefOr, ResponseBuilder};
    const CSRF: &str = "A write sent with a session cookie but without a matching `Origin` or `Referer` is refused.";
    for (path, item) in doc.paths.paths.iter_mut() {
        if !path.starts_with("/api/") {
            continue;
        }
        let ops = [
            (Method::GET, item.get.as_mut()),
            (Method::PUT, item.put.as_mut()),
            (Method::POST, item.post.as_mut()),
            (Method::DELETE, item.delete.as_mut()),
            (Method::PATCH, item.patch.as_mut()),
        ];
        for (method, op) in ops {
            let Some(op) = op else { continue };
            if !crate::api::is_streamed_upload(&method, path) {
                op.responses
                    .responses
                    .entry("504".to_string())
                    .or_insert_with(|| {
                        ResponseBuilder::new()
                            .description("The request ran past the API's time budget.")
                            .content(
                                "text/plain",
                                ContentBuilder::new()
                                    .schema(Some(ObjectBuilder::new().schema_type(Type::String)))
                                    .build(),
                            )
                            .build()
                            .into()
                    });
            }
            let writes = matches!(
                method,
                Method::PUT | Method::POST | Method::DELETE | Method::PATCH
            );
            if !writes || crate::csrf::is_csrf_exempt(path) || !accepts_cookie(op) {
                continue;
            }
            match op.responses.responses.get_mut("403") {
                Some(RefOr::T(response)) => {
                    response.description = format!("{} {CSRF}", response.description);
                }
                Some(RefOr::Ref(_)) => {}
                None => {
                    op.responses.responses.insert(
                        "403".to_string(),
                        ResponseBuilder::new().description(CSRF).build().into(),
                    );
                }
            }
        }
    }
}

/// The error slug of the refusal [`crate::totp::enrollment_blocks`] produces.
const MFA_ENROLLMENT_REQUIRED: &str = "mfa_enrollment_required";

/// The message of that refusal, as [`crate::auth`] writes it.
const MFA_ENROLLMENT_MESSAGE: &str = "This server requires a passkey or authenticator app. Set one up to continue, or log out and finish later.";

/// States the 403 a route behind a session-resolving access layer may
/// answer before its handler runs: when `daemon.auth.require_second_factor`
/// is on, the layer ([`crate::auth`]) refuses a signed-in user who has
/// enrolled neither a passkey nor an authenticator app with
/// `mfa_enrollment_required`. [`add_group`] calls this for every route of a
/// group whose access [`Access::gates_enrollment`] (the legacy unprefixed
/// routes included), except the ones [`crate::totp::enrollment_exempt`]
/// keeps open for enrolling and signing out.
///
/// A route without a 403 gets one with the `ErrorJson` body and the
/// enrollment refusal as a named example, beside the middleware's
/// `forbidden` when `generic_forbidden` says the route can answer one; a
/// route whose 403 already carries the standard error body gets the
/// example added beside its own (a single `example` becomes a named one,
/// under its slug); a route whose 403 has a body of its own keeps it and
/// states `ErrorJson` as the other shape (`anyOf`, since a body with only
/// `error` fits both), since the refusal
/// arrives before the handler and in the middleware's words. A route that
/// already names the refusal is left as it is.
///
/// # Arguments
///
/// * `op` - The operation, before [`fill_untyped_content`] types its bare
///   error responses.
/// * `generic_forbidden` - Whether a layer can answer the route with the
///   middleware's plain `forbidden` (a role check, or the CSRF check on a
///   write); false for a safe method behind the any-session layer.
fn add_enrollment_refusal(op: &mut Operation, generic_forbidden: bool) {
    use axum::http::StatusCode;
    use utoipa::openapi::example::ExampleBuilder;
    use utoipa::openapi::schema::{AnyOfBuilder, Schema};
    use utoipa::openapi::{Content, ContentBuilder, Ref, RefOr, ResponseBuilder};
    const SENTENCE: &str =
        "A signed-in user who must enroll a second factor first gets `mfa_enrollment_required`.";
    let example_named = |name: &str, summary: &str, value: serde_json::Value| {
        (
            name.to_string(),
            RefOr::T(
                ExampleBuilder::new()
                    .summary(summary)
                    .value(Some(value))
                    .build(),
            ),
        )
    };
    let standard_body = |schema: &Option<RefOr<Schema>>| match schema {
        Some(RefOr::Ref(reference)) => {
            let name = reference
                .ref_location
                .rsplit('/')
                .next()
                .unwrap_or_default();
            name == API_ERROR || name == "ErrorJson"
        }
        _ => false,
    };
    let response = match op.responses.responses.get_mut("403") {
        Some(RefOr::T(response)) => response,
        Some(RefOr::Ref(_)) => return,
        None => {
            op.responses.responses.insert(
                "403".to_string(),
                ResponseBuilder::new().description(SENTENCE).build().into(),
            );
            match op.responses.responses.get_mut("403") {
                Some(RefOr::T(response)) => response,
                _ => return,
            }
        }
    };
    if !response.description.contains(MFA_ENROLLMENT_REQUIRED) {
        response.description = format!("{} {SENTENCE}", response.description.trim_end());
    }
    let json = response
        .content
        .iter_mut()
        .find(|(media, _)| media.starts_with("application/json"))
        .map(|(_, content)| content);
    let content: &mut Content = match json {
        Some(RefOr::T(content)) => content,
        Some(RefOr::Ref(_)) => return,
        None => {
            let (name, forbidden) = example_named(
                "forbidden",
                "The caller may not do this",
                serde_json::to_value(crate::http_error::ErrorJson::for_status(
                    StatusCode::FORBIDDEN,
                ))
                .unwrap_or_default(),
            );
            let mut built = ContentBuilder::new()
                .schema(Some(Ref::from_schema_name("ErrorJson")))
                .build();
            if generic_forbidden {
                built.examples.insert(name, forbidden);
            }
            response
                .content
                .insert("application/json".to_string(), RefOr::T(built));
            match response.content.get_mut("application/json") {
                Some(RefOr::T(content)) => content,
                _ => return,
            }
        }
    };
    if content.examples.contains_key(MFA_ENROLLMENT_REQUIRED) {
        return;
    }
    if !standard_body(&content.schema) {
        if let Some(own) = content.schema.take() {
            content.schema = Some(RefOr::T(Schema::AnyOf(
                AnyOfBuilder::new()
                    .item(own)
                    .item(Ref::from_schema_name("ErrorJson"))
                    .build(),
            )));
        }
    }
    if let Some(own) = content.example.take() {
        // The body's slug names the example when it is one; a handler
        // whose `error` is a sentence gets `route`.
        let slug = own.get("error").and_then(serde_json::Value::as_str);
        let name = slug
            .filter(|slug| {
                !slug.is_empty()
                    && slug.len() <= 40
                    && slug
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
            .unwrap_or("route")
            .to_string();
        let (name, example) = example_named(&name, "The route's own refusal", own);
        content.examples.insert(name, example);
    }
    let (name, example) = example_named(
        MFA_ENROLLMENT_REQUIRED,
        "The user must enroll a second factor first",
        serde_json::to_value(crate::http_error::ErrorJson::new(
            StatusCode::FORBIDDEN,
            MFA_ENROLLMENT_REQUIRED,
            MFA_ENROLLMENT_MESSAGE,
        ))
        .unwrap_or_default(),
    );
    content.examples.insert(name, example);
}

/// True when a session cookie can be the operation's credential: it names a
/// cookie scheme, or it is open (a browser still sends its cookies).
///
/// # Arguments
///
/// * `op` - The operation, with its security stated.
///
/// # Returns
///
/// Whether the CSRF check can apply.
fn accepts_cookie(op: &Operation) -> bool {
    let requirements = op.security.as_deref().unwrap_or_default();
    requirements.is_empty()
        || requirements.iter().any(|requirement| {
            serde_json::to_value(requirement)
                .ok()
                .and_then(|value| value.as_object().cloned())
                .is_some_and(|schemes| {
                    schemes.is_empty()
                        || schemes.contains_key(OPERATOR_SESSION)
                        || schemes.contains_key(PORTAL_SESSION)
                })
        })
}

/// Gives every request or response media type without a schema one, so
/// generated clients keep the body: any JSON value for JSON (a body not
/// typed yet), bytes for everything else. An error response declared
/// without a body gets [`API_ERROR`]: the `ErrorJson` that
/// [`crate::http_error::brand_error_responses`] writes for a bare status,
/// or a handler's own `{"error", …}` body.
///
/// # Arguments
///
/// * `doc` - The document, after every group is added.
fn fill_untyped_content(doc: &mut OpenApi) {
    use utoipa::openapi::schema::{ObjectBuilder, SchemaFormat, SchemaType, Type};
    use utoipa::openapi::{ContentBuilder, KnownFormat, Ref, RefOr};
    let fill = |media: &str, content: &mut utoipa::openapi::Content| {
        if content.schema.is_some() {
            return;
        }
        let schema = if media.starts_with("application/json") {
            ObjectBuilder::new().schema_type(SchemaType::AnyValue)
        } else {
            ObjectBuilder::new()
                .schema_type(Type::String)
                .format(Some(SchemaFormat::KnownFormat(KnownFormat::Binary)))
        };
        content.schema = Some(RefOr::T(schema.build().into()));
    };
    for item in doc.paths.paths.values_mut() {
        for op in operations_mut(item) {
            if let Some(RefOr::T(body)) = op.request_body.as_mut() {
                for (media, content) in body.content.iter_mut() {
                    if let RefOr::T(content) = content {
                        fill(media, content);
                    }
                }
            }
            for (status, response) in op.responses.responses.iter_mut() {
                if let RefOr::T(response) = response {
                    let is_error = status.starts_with('4') || status.starts_with('5');
                    if is_error && response.content.is_empty() {
                        response.content.insert(
                            "application/json".to_string(),
                            RefOr::T(
                                ContentBuilder::new()
                                    .schema(Some(Ref::from_schema_name(API_ERROR)))
                                    .build(),
                            ),
                        );
                    }
                    for (media, content) in response.content.iter_mut() {
                        if let RefOr::T(content) = content {
                            fill(media, content);
                        }
                    }
                }
            }
        }
    }
}

/// A request body of raw bytes (a file upload), documented as
/// `type: string, format: binary` so generated clients send the bytes
/// rather than a JSON array.
#[derive(utoipa::ToSchema)]
#[schema(value_type = String, format = Binary)]
pub struct BinaryBody(
    /// The body's bytes.
    #[allow(dead_code)]
    Vec<u8>,
);

/// The schema of a placement's `replicas`: a replica count (0 stops every
/// replica) or `"all"`, as [`bookclerk_library::control_plane::Replicas`]
/// serializes.
///
/// # Returns
///
/// A `oneOf` of a non-negative integer and the string `all`.
pub fn replicas_schema() -> utoipa::openapi::schema::Schema {
    use utoipa::openapi::schema::{ObjectBuilder, OneOfBuilder, Schema, Type};
    let count = ObjectBuilder::new()
        .schema_type(Type::Integer)
        .minimum(Some(0));
    let all = ObjectBuilder::new()
        .schema_type(Type::String)
        .enum_values(Some(["all"]));
    Schema::OneOf(
        OneOfBuilder::new()
            .item(count)
            .item(all)
            .description(Some(
                "A replica count (0 stops every replica) or `\"all\"`.",
            ))
            .build(),
    )
}

/// One setting a plugin declares in its `plugin.toml` `[[settings]]`:
/// its key, value type, default and help text.
//
// This is the OpenAPI mirror of `bookclerk_plugin_abi::SettingSpec`, whose
// crate carries no schema derive. The fields and their serialized names
// match the ABI type; the struct exists only to describe it in the
// document and is never constructed.
#[allow(dead_code)] // Described, never constructed: `SettingSpec` is the wire type.
#[derive(utoipa::ToSchema)]
#[schema(as = SettingSpec)]
pub(crate) struct SettingSpecDoc {
    /// Setting key (`[a-z][a-z0-9_]*`, at most 64 bytes).
    #[schema(example = "root")]
    key: String,
    /// Operator-facing label; omitted when the key is shown.
    #[schema(example = "Library root")]
    label: Option<String>,
    /// Value type: `string`, `bool`, `integer`, `enum` or `secret`.
    #[schema(rename = "type", example = "string")]
    kind: SettingKindDoc,
    /// Allowed values of an `enum` setting; omitted for other kinds.
    values: Option<Vec<SettingValueSpecDoc>>,
    /// Default applied when the instance document has no value; omitted
    /// when there is none.
    #[schema(schema_with = setting_default_schema)]
    default: Option<serde_json::Value>,
    /// True when the instance cannot run without a value; omitted when
    /// false.
    #[schema(example = true)]
    required: Option<bool>,
    /// Help text shown beside the setting; omitted when there is none.
    #[schema(example = "Directory the finished audiobooks are written to.")]
    description: Option<String>,
}

/// Value type of a declared plugin setting.
//
// Mirror of `bookclerk_plugin_abi::SettingKind`; see `SettingSpecDoc`.
#[allow(dead_code)] // Described, never constructed: `SettingKind` is the wire type.
#[derive(utoipa::ToSchema)]
#[schema(as = SettingKind, rename_all = "snake_case", example = "string")]
pub(crate) enum SettingKindDoc {
    /// A UTF-8 string of at most 1024 bytes.
    String,
    /// A boolean.
    Bool,
    /// A 64-bit signed integer.
    Integer,
    /// One of the declared `values`.
    Enum,
    /// A secret stored through the secrets binding, never in the document.
    Secret,
}

/// One allowed value of an `enum` setting: the stored id and its label.
//
// Mirror of `bookclerk_plugin_abi::SettingValueSpec`; see `SettingSpecDoc`.
#[allow(dead_code)] // Described, never constructed: `SettingValueSpec` is the wire type.
#[derive(utoipa::ToSchema)]
#[schema(as = SettingValueSpec)]
pub(crate) struct SettingValueSpecDoc {
    /// Stored value.
    #[schema(example = "sync_missing")]
    id: String,
    /// Operator-facing label; omitted when the id is shown.
    #[schema(example = "Sync missing")]
    label: Option<String>,
}

/// Schema of a declared setting's `default`: any JSON scalar, since the
/// value's type follows the setting's `type`.
fn setting_default_schema() -> utoipa::openapi::schema::Schema {
    use utoipa::openapi::schema::{ObjectBuilder, SchemaType};
    ObjectBuilder::new()
        .schema_type(SchemaType::AnyValue)
        .description(Some(
            "Default applied when the instance document has no value; omitted when there is \
             none. A string, boolean or integer, as the setting's `type` says.",
        ))
        .examples([serde_json::json!("/srv/audiobooks")])
        .build()
        .into()
}

/// The document, built once per process and shared by every router.
///
/// # Returns
///
/// The same [`document`] each call.
pub fn shared_document() -> Arc<OpenApi> {
    static DOCUMENT: std::sync::OnceLock<Arc<OpenApi>> = std::sync::OnceLock::new();
    DOCUMENT.get_or_init(|| Arc::new(document())).clone()
}

/// An OpenAPI document as this daemon serves it: the `openapi` version
/// string, `info` about the daemon, every route under `paths` and the
/// schemas under `components`.
pub struct OpenApiDocument;

impl utoipa::PartialSchema for OpenApiDocument {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::schema::ObjectBuilder::new()
            .description(Some(
                "An OpenAPI document as this daemon serves it: the `openapi` version string, \
                 `info` about the daemon, every route under `paths` and the schemas under \
                 `components`.",
            ))
            .examples([serde_json::json!({
                "openapi": "3.1.0",
                "info": {"title": "Bookclerk daemon API", "version": "0.1.0"},
                "paths": {"/health": {"get": {"responses": {"200": {"description": "The daemon is up."}}}}},
                "components": {"schemas": {}}
            })])
            .build()
            .into()
    }
}

impl utoipa::ToSchema for OpenApiDocument {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("OpenApiDocument")
    }
}

/// Serves the OpenAPI document.
#[utoipa::path(
    get,
    path = "/api/openapi.json",
    responses((status = 200, description = "The OpenAPI 3.1 document for this daemon's version.", body = OpenApiDocument)),
)]
pub async fn get_openapi(
    axum::extract::State(doc): axum::extract::State<Arc<OpenApi>>,
) -> Json<Arc<OpenApi>> {
    Json(doc)
}

/// The document restated as OpenAPI 3.0.3, or why it could not be built.
pub type V30Document = Arc<Result<v30::Json, String>>;

/// Restates a document as OpenAPI 3.0.3.
///
/// # Arguments
///
/// * `doc` - The OpenAPI 3.1 document.
///
/// # Returns
///
/// The 3.0.3 document, keys in the 3.1 document's order.
///
/// # Errors
///
/// Fails when the document uses an OpenAPI 3.1 form [`v30::downgrade`] has
/// no 3.0 equivalent for, or cannot be serialized; either is a bug the
/// tests catch.
fn document_v30(doc: &OpenApi) -> anyhow::Result<v30::Json> {
    // Through text, not `serde_json::Value`, whose maps would sort the keys.
    let text = serde_json::to_string(doc)?;
    let tree: v30::Json = serde_json::from_str(&text)?;
    let mut restated = v30::downgrade(&tree)?;
    // The shared `OpenApiDocument` example shows the 3.1 document; the
    // restated document's copy shows itself.
    if let Some(version) = [
        "components",
        "schemas",
        "OpenApiDocument",
        "example",
        "openapi",
    ]
    .iter()
    .try_fold(&mut restated, |node, key| node.get_mut(key))
    {
        *version = v30::Json::String("3.0.3".to_string());
    }
    Ok(restated)
}

/// The 3.0.3 document, built once per process (when the router is built)
/// from [`shared_document`].
///
/// # Returns
///
/// The same document each call, or the error that stopped it, which is
/// logged once.
pub fn shared_v30_document() -> V30Document {
    static DOCUMENT: std::sync::OnceLock<V30Document> = std::sync::OnceLock::new();
    DOCUMENT
        .get_or_init(|| {
            let built = document_v30(&shared_document()).map_err(|err| err.to_string());
            if let Err(err) = &built {
                tracing::error!(error = %err, "the OpenAPI 3.0 document could not be built");
            }
            Arc::new(built)
        })
        .clone()
}

/// Serves the OpenAPI document restated as OpenAPI 3.0.3.
#[utoipa::path(
    get,
    path = "/api/openapi-3.0.json",
    description = "The same API as `GET /api/openapi.json`, restated in OpenAPI 3.0.3 form for \
                   tools that only read 3.0, such as Cloudflare API Shield's schema validation: \
                   nullable types use `nullable: true`, and a nullable reference is a copy of \
                   the schema it names.",
    responses(
        (status = 200, description = "The OpenAPI 3.0.3 document for this daemon's version.", body = OpenApiDocument),
        (status = 500, description = "The document could not be restated; this is a bug.", body = OpenApiV30Unavailable, example = json!({"error": "openapi_v30_unavailable", "message": "The OpenAPI 3.0 document could not be built; see the daemon log."})),
    ),
)]
pub async fn get_openapi_v30(
    axum::extract::State(doc): axum::extract::State<V30Document>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match doc.as_ref() {
        Ok(value) => Json(value).into_response(),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(OpenApiV30Unavailable {
                error: "openapi_v30_unavailable",
                message: "The OpenAPI 3.0 document could not be built; see the daemon log.",
            }),
        )
            .into_response(),
    }
}

/// Body of the `500` from `GET /api/openapi-3.0.json` when the 3.1
/// document could not be restated as 3.0.3.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct OpenApiV30Unavailable {
    /// Always `openapi_v30_unavailable`.
    #[schema(example = "openapi_v30_unavailable")]
    pub error: &'static str,
    /// What to do: the daemon log names the form that could not be restated.
    #[schema(example = "The OpenAPI 3.0 document could not be built; see the daemon log.")]
    pub message: &'static str,
}

/// The document as pretty JSON, as `bookclerkd --print-openapi` prints it
/// and `crates/bookclerkd/openapi.json` holds it.
///
/// # Returns
///
/// The JSON text with a trailing newline.
///
/// # Errors
///
/// Fails only if the document cannot be serialized, which would be a bug.
pub fn document_json() -> serde_json::Result<String> {
    serde_json::to_string_pretty(&document()).map(|mut s| {
        s.push('\n');
        s
    })
}

/// The document restated as OpenAPI 3.0.3, as pretty JSON: what
/// `bookclerkd --print-openapi 3.0` prints and
/// `crates/bookclerkd/openapi-3.0.json` holds.
///
/// # Returns
///
/// The JSON text with a trailing newline.
///
/// # Errors
///
/// Fails when the document uses an OpenAPI 3.1 form [`v30::downgrade`] has
/// no 3.0 equivalent for, or cannot be serialized; either is a bug the
/// tests catch.
pub fn document_v30_json() -> anyhow::Result<String> {
    let mut text = serde_json::to_string_pretty(&document_v30(&document())?)?;
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[tokio::test]
    async fn v30_route_reports_a_document_that_could_not_be_built() {
        use http_body_util::BodyExt;
        let doc: V30Document = Arc::new(Err("boom".to_string()));
        let response = get_openapi_v30(axum::extract::State(doc)).await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"], "openapi_v30_unavailable");
        assert!(body["message"].as_str().unwrap().contains("3.0"));
    }
    use std::path::Path;

    /// HTTP methods an OpenAPI path item can hold, in JSON key form.
    const METHODS: &[&str] = &[
        "get", "put", "post", "delete", "options", "head", "patch", "trace",
    ];

    /// Plain axum registrations that stay out of the document, as
    /// `file: snippet` pairs: the invite page and the SPA's static files.
    const UNDOCUMENTED_ROUTES: &[(&str, &str)] = &[
        ("src/api.rs", ".route(\"/invite\", get(invite_page))"),
        ("src/api.rs", ".route_service(path, spa_index.clone())"),
        ("src/api.rs", ".fallback_service(spa_files)"),
    ];

    /// The document as JSON, for walking.
    fn document_value() -> serde_json::Value {
        serde_json::to_value(document()).expect("the document serializes")
    }

    /// Every `(method, path, operation)` in a JSON document.
    fn operations(doc: &serde_json::Value) -> Vec<(String, String, serde_json::Value)> {
        let mut out = Vec::new();
        for (path, item) in doc["paths"].as_object().expect("paths").iter() {
            for method in METHODS {
                if let Some(op) = item.get(*method) {
                    out.push((method.to_string(), path.clone(), op.clone()));
                }
            }
        }
        out
    }

    /// Collects every `$ref` string under a JSON value.
    fn refs(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, inner) in map {
                    if key == "$ref" {
                        if let Some(target) = inner.as_str() {
                            out.push(target.to_string());
                        }
                    }
                    refs(inner, out);
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| refs(v, out)),
            _ => {}
        }
    }

    /// `crates/bookclerkd/openapi.json` matches the code, so reviewers see
    /// API changes in the diff and the docs pipeline renders the real API.
    #[test]
    fn the_committed_document_is_current() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("openapi.json");
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        let current = document_json().expect("the document serializes");
        // A version bump alone does not make the committed copy stale.
        let without_version = |text: &str| {
            let mut value: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
            if let Some(info) = value.get_mut("info").and_then(|i| i.as_object_mut()) {
                info.remove("version");
            }
            value
        };
        assert!(
            without_version(&committed) == without_version(&current),
            "{} is stale; regenerate it with \
             `cargo run -p bookclerkd -- --print-openapi > crates/bookclerkd/openapi.json`",
            path.display()
        );
    }

    /// `crates/bookclerkd/openapi-3.0.json` matches the code too.
    #[test]
    fn the_committed_v30_document_is_current() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("openapi-3.0.json");
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        let current = document_v30_json().expect("the document restates as 3.0");
        let without_version = |text: &str| {
            let mut value: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
            if let Some(info) = value.get_mut("info").and_then(|i| i.as_object_mut()) {
                info.remove("version");
            }
            value
        };
        assert!(
            without_version(&committed) == without_version(&current),
            "{} is stale; regenerate it with \
             `cargo run -p bookclerkd -- --print-openapi 3.0 > crates/bookclerkd/openapi-3.0.json`",
            path.display()
        );
    }

    /// The 3.0 document holds no OpenAPI 3.1-only form anywhere, checked
    /// by walking the whole JSON rather than by the translation's own rules,
    /// and it describes the same operations and schemas as the 3.1 one.
    #[test]
    fn the_v30_document_is_openapi_3_0() {
        /// Keywords that only exist in OpenAPI 3.1 / JSON Schema 2020-12.
        const ONLY_3_1: &[&str] = &[
            "const",
            "examples",
            "prefixItems",
            "propertyNames",
            "contentMediaType",
            "contentEncoding",
            "$schema",
            "$id",
            "$defs",
            "if",
            "then",
            "else",
            "dependentRequired",
            "dependentSchemas",
            "unevaluatedProperties",
            "unevaluatedItems",
            "patternProperties",
            "webhooks",
            "jsonSchemaDialect",
        ];
        const TYPES: &[&str] = &["string", "number", "integer", "boolean", "array", "object"];
        fn check(value: &serde_json::Value, at: &str, problems: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(map) => {
                    if map.contains_key("$ref") && map.len() > 1 {
                        problems.push(format!("{at}: `$ref` with siblings"));
                    }
                    for (key, inner) in map {
                        let here = format!("{at}/{key}");
                        // `properties` maps and examples hold names and data, not keywords.
                        // A Media Type Object's `examples` map (Example Objects) is
                        // 3.0 too; only a schema's `examples` list is 3.1-only.
                        let media_examples =
                            key == "examples" && at.rsplit('/').nth(2) == Some("content");
                        let names_or_data = at.ends_with("/properties")
                            || at.ends_with("/schemas")
                            || key == "example"
                            || media_examples;
                        if !names_or_data {
                            if ONLY_3_1.contains(&key.as_str()) {
                                problems.push(format!("{here}: 3.1-only keyword"));
                            }
                            if key == "type" {
                                match inner.as_str() {
                                    Some(t) if TYPES.contains(&t) => {}
                                    _ => problems.push(format!("{here}: not a 3.0 type")),
                                }
                            }
                            if key == "nullable" && !inner.is_boolean() {
                                problems.push(format!("{here}: not a boolean"));
                            }
                        }
                        // Security schemes have a `type` of their own (`http`, `apiKey`).
                        if key != "example" && !media_examples && key != "securitySchemes" {
                            check(inner, &here, problems);
                        }
                    }
                }
                serde_json::Value::Array(items) => {
                    for (i, item) in items.iter().enumerate() {
                        check(item, &format!("{at}/{i}"), problems);
                    }
                }
                _ => {}
            }
        }
        let v31 = document_value();
        let v30: serde_json::Value =
            serde_json::from_str(&document_v30_json().expect("restates")).expect("JSON");
        assert_eq!(v30["openapi"], "3.0.3");
        for name in CLOSED_OUTPUT_SCHEMAS {
            for doc in [&v31, &v30] {
                assert_eq!(
                    doc["components"]["schemas"][*name]["additionalProperties"], false,
                    "{name} must publish additionalProperties: false"
                );
            }
        }
        // The shared `OpenApiDocument` example describes each document as itself.
        assert_eq!(
            v31["components"]["schemas"]["OpenApiDocument"]["examples"][0]["openapi"],
            "3.1.0"
        );
        assert_eq!(
            v30["components"]["schemas"]["OpenApiDocument"]["example"]["openapi"],
            "3.0.3"
        );
        let mut problems = Vec::new();
        check(&v30, "", &mut problems);
        assert!(problems.is_empty(), "{problems:#?}");
        let ids = |doc: &serde_json::Value| {
            operations(doc)
                .into_iter()
                .map(|(m, p, op)| (m, p, op["operationId"].clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&v30), ids(&v31));
        let names = |doc: &serde_json::Value| {
            doc["components"]["schemas"]
                .as_object()
                .expect("schemas")
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&v30), names(&v31));
    }

    /// A route without an auth layer that refuses callers who are not signed
    /// in states the credentials it reads, so clients send them.
    #[test]
    fn handler_checked_routes_state_their_security() {
        let doc = document_value();
        for (method, path, op) in operations(&doc) {
            let refuses_anonymous = [
                "The caller is not signed in.",
                "The Bearer token is missing or wrong.",
                "The node signature is missing or wrong.",
                "The upload token is missing or wrong.",
                "The access token is missing or invalid.",
            ]
            .iter()
            .any(|d| op["responses"]["401"]["description"] == *d);
            if refuses_anonymous {
                assert!(
                    op["security"].as_array().is_some_and(|s| !s.is_empty()),
                    "{method} {path} needs a signed-in caller but its security is empty"
                );
            }
        }
    }

    /// The outer layers' refusals are stated: the API timeout's 504 on every
    /// `/api/` route but the streamed upload, and the CSRF 403 on cookie
    /// writes but not on the exempt sign-in routes.
    #[test]
    fn middleware_refusals_are_stated() {
        let doc = document_value();
        for (method, path, op) in operations(&doc) {
            if !path.starts_with("/api/") {
                continue;
            }
            let streamed =
                method == "put" && path == "/api/node/v1/uploads/{upload_id}/files/{index}";
            assert_eq!(
                op["responses"].get("504").is_some(),
                !streamed,
                "{method} {path} 504"
            );
        }
        let csrf = |method: &str, path: &str| {
            doc["paths"][path][method]["responses"]["403"]["description"]
                .as_str()
                .is_some_and(|d| d.contains("`Origin` or `Referer`"))
        };
        assert!(csrf("patch", "/api/preferences"));
        assert!(csrf("post", "/api/auth/logout"));
        assert!(!csrf("post", "/api/auth/login"));
        assert!(!csrf("get", "/api/preferences"));
    }

    /// Every route behind a session-resolving access layer, the legacy
    /// unprefixed ones included, states the `mfa_enrollment_required`
    /// refusal on its 403 with an example and keeps its own body beside the
    /// middleware's; the enrollment and sign-out routes, the public routes
    /// and the operator-only routes do not.
    #[test]
    fn enrollment_refusals_are_stated() {
        // The predicate itself, for every access variant.
        for (access, gates) in [
            (Access::Public, false),
            (Access::Operator, false),
            (Access::OperatorOrAdministrator, true),
            (Access::OperatorOrOwner, true),
            (Access::OperatorOrNodeOwner, true),
            (Access::OperatorOrUser, true),
            (Access::OperatorOrPortal, true),
        ] {
            assert_eq!(access.gates_enrollment(), gates, "{access:?}");
        }
        let doc = document_value();
        let forbidden =
            |method: &str, path: &str| doc["paths"][path][method]["responses"]["403"].clone();
        let names_refusal = |response: &serde_json::Value| {
            response["description"]
                .as_str()
                .is_some_and(|d| d.contains("mfa_enrollment_required"))
        };
        for (method, path) in [
            ("get", "/api/wishlist"),
            ("post", "/api/library/scan"),
            ("get", "/api/library/books"),
            ("get", "/api/status"),
            ("get", "/api/config/nodes"),
            ("get", "/api/auth/oidc/clients"),
            ("get", "/api/auth/branch-consent"),
            ("get", "/status"),
            ("post", "/scan"),
        ] {
            let response = forbidden(method, path);
            assert!(names_refusal(&response), "{method} {path} 403: {response}");
            let example = &response["content"]["application/json"]["examples"]
                ["mfa_enrollment_required"]["value"];
            assert_eq!(
                example["error"], "mfa_enrollment_required",
                "{method} {path} 403 example"
            );
            assert_eq!(example["status"], 403, "{method} {path} 403 example");
        }
        // A safe method behind the any-session layer has no generic
        // `forbidden` (that layer checks no role); its writes keep it for
        // the CSRF refusal, and role-checked groups keep it everywhere.
        let examples = |method: &str, path: &str| {
            forbidden(method, path)["content"]["application/json"]["examples"]
                .as_object()
                .map(|examples| examples.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        };
        assert_eq!(
            examples("get", "/api/wishlist"),
            ["mfa_enrollment_required"]
        );
        assert_eq!(
            examples("post", "/api/wishlist"),
            ["forbidden", "mfa_enrollment_required"]
        );
        assert_eq!(
            examples("get", "/api/status"),
            ["forbidden", "mfa_enrollment_required"]
        );
        // Exempt, public and operator-only routes keep their own 403 or none.
        assert!(forbidden("get", "/api/auth/me").is_null());
        assert!(forbidden("get", "/health").is_null());
        assert!(!names_refusal(&forbidden("get", "/api/auth/totp")));
        assert!(
            forbidden("get", "/api/auth/oidc/elevate")["content"]["application/json"]["examples"]
                .is_null()
        );
        assert!(!names_refusal(&forbidden("get", "/api/auth/oidc/elevate")));
        assert!(!names_refusal(&forbidden("get", "/api/settings")));
        // A route with a body of its own states both shapes, and its own example by its slug.
        let scan = forbidden("post", "/api/library/scan");
        let scan = &scan["content"]["application/json"];
        let any_of = scan["schema"]["anyOf"].as_array().expect("anyOf");
        assert_eq!(any_of.len(), 2);
        assert_eq!(any_of[0]["$ref"], "#/components/schemas/ActionResponse");
        assert_eq!(any_of[1]["$ref"], "#/components/schemas/ErrorJson");
        assert_eq!(scan["examples"]["forbidden"]["value"]["error"], "forbidden");
        assert!(scan.get("example").is_none());
        // A route that already names the refusal is left as it is: one example, one mention.
        let clients = forbidden("get", "/api/auth/oidc/clients");
        assert_eq!(
            clients["description"]
                .as_str()
                .unwrap_or_default()
                .matches("mfa_enrollment_required")
                .count(),
            1
        );
        assert_eq!(
            clients["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ErrorJson"
        );
        assert_eq!(
            clients["content"]["application/json"]["examples"]
                .as_object()
                .map(|m| m.len()),
            Some(2)
        );
        // A text body keeps its media type and gains the JSON one.
        let cancel = forbidden("delete", "/api/wishlist/{uuid}");
        assert!(cancel["content"]["text/plain"].is_object());
        assert_eq!(
            cancel["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ErrorJson"
        );
        // A route whose `error` is a sentence names its own example `route`.
        let runtime = forbidden("put", "/api/config/nodes/{node_id}/runtime");
        assert!(
            runtime["content"]["application/json"]["examples"]["route"].is_object(),
            "{runtime}"
        );
        // A write without a 403 before gets the middleware's own refusal too.
        let wishlist = forbidden("post", "/api/wishlist");
        assert_eq!(
            wishlist["content"]["application/json"]["examples"]["forbidden"]["value"]["status"],
            403
        );
    }

    /// Every request and response media type carries a schema, so generated
    /// clients keep the body instead of modelling it as empty.
    #[test]
    fn every_media_type_has_a_schema() {
        let doc = document_value();
        for (method, path, op) in operations(&doc) {
            let bodies = std::iter::once(("request", &op["requestBody"])).chain(
                op["responses"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(s, r)| (s.as_str(), r)),
            );
            for (status, body) in bodies {
                for (media, content) in body["content"].as_object().into_iter().flatten() {
                    assert!(
                        content.get("schema").is_some(),
                        "{method} {path} {status} {media} has no schema"
                    );
                }
            }
        }
    }

    /// Every JSON body is typed: no request or response is left to the
    /// any-value schema [`fill_untyped_content`] gives a body without one,
    /// so generated clients and the rendered reference know its shape.
    #[test]
    fn json_bodies_are_typed() {
        let doc = document_value();
        let mut untyped = Vec::new();
        for (method, path, op) in operations(&doc) {
            let bodies = std::iter::once(("request", &op["requestBody"])).chain(
                op["responses"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(s, r)| (s.as_str(), r)),
            );
            for (status, body) in bodies {
                for (media, content) in body["content"].as_object().into_iter().flatten() {
                    let any_value = content["schema"]
                        .as_object()
                        .is_some_and(|schema| schema.is_empty());
                    if media.starts_with("application/json") && any_value {
                        untyped.push(format!("{method} {path} {status}"));
                    }
                }
            }
        }
        assert!(
            untyped.is_empty(),
            "JSON bodies without a schema: {untyped:?}"
        );
    }

    /// Whether a schema is composed of others (`$ref`, `allOf`, `oneOf`,
    /// `anyOf`), or lists such items, so its example comes from them.
    fn composed(schema: &serde_json::Value) -> bool {
        let composes = |value: &serde_json::Value| {
            ["$ref", "allOf", "oneOf", "anyOf"]
                .iter()
                .any(|key| value.get(key).is_some())
        };
        composes(schema) || schema.get("items").is_some_and(composes)
    }

    /// Whether a schema needs no example of its own: it carries one, it is
    /// composed of schemas that do ([`composed`]), or its `enum` has one
    /// value (a tag such as `kind` or `method`), which is its example.
    fn exemplified(schema: &serde_json::Value) -> bool {
        schema.get("example").is_some()
            || schema.get("examples").is_some()
            || composed(schema)
            || schema["enum"]
                .as_array()
                .is_some_and(|values| values.len() == 1)
    }

    /// Collects the properties under `schema` without an example, walking
    /// the properties of every nested object, the branches of `oneOf`,
    /// `allOf` and `anyOf`, array items and map values.
    fn properties_without_example(at: &str, schema: &serde_json::Value, out: &mut Vec<String>) {
        for (name, property) in schema["properties"].as_object().into_iter().flatten() {
            let at = format!("{at}.{name}");
            if !exemplified(property) {
                out.push(at.clone());
            }
            properties_without_example(&at, property, out);
        }
        for key in ["oneOf", "allOf", "anyOf"] {
            for (index, branch) in schema[key].as_array().into_iter().flatten().enumerate() {
                properties_without_example(&format!("{at}.{key}[{index}]"), branch, out);
            }
        }
        if schema["items"].is_object() {
            properties_without_example(&format!("{at}[]"), &schema["items"], out);
        }
        if schema["additionalProperties"].is_object() {
            properties_without_example(&format!("{at}.*"), &schema["additionalProperties"], out);
        }
    }

    /// Every property of every schema, in the components and inline in a
    /// body, nested objects and the branches of a composed schema included,
    /// carries an `example` or `examples`, unless it is composed of other
    /// schemas (which carry their own) or is a one-value `enum`, so the
    /// rendered reference can show a complete example for every body.
    #[test]
    fn every_property_carries_an_example() {
        let doc = document_value();
        let mut missing = Vec::new();
        for (name, schema) in doc["components"]["schemas"]
            .as_object()
            .into_iter()
            .flatten()
        {
            properties_without_example(name, schema, &mut missing);
        }
        for (method, path, op) in operations(&doc) {
            let bodies = std::iter::once(("request", &op["requestBody"])).chain(
                op["responses"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(s, r)| (s.as_str(), r)),
            );
            for (status, body) in bodies {
                for (media, content) in body["content"].as_object().into_iter().flatten() {
                    properties_without_example(
                        &format!("{method} {path} {status} {media}"),
                        &content["schema"],
                        &mut missing,
                    );
                }
            }
        }
        assert!(
            missing.is_empty(),
            "properties without an example: {missing:?}"
        );
    }

    /// The walk reaches a property inside a `oneOf` branch, an `allOf`
    /// part, array items and map values, and accepts a one-value `enum`.
    #[test]
    fn nested_properties_are_checked() {
        let schema = serde_json::json!({
            "oneOf": [
                { "type": "object", "properties": {
                    "method": { "type": "string", "enum": ["totp"] },
                    "code": { "type": "string" }
                } },
                { "allOf": [
                    { "$ref": "#/components/schemas/Other" },
                    { "type": "object", "properties": { "extra": { "type": "integer" } } }
                ] }
            ],
            "properties": {
                "list": { "type": "array", "items": { "type": "object", "properties": {
                    "deep": { "type": "string", "example": "x" },
                    "deeper": { "type": "string" }
                } } },
                "map": { "type": "object", "additionalProperties": { "type": "object", "properties": {
                    "value": { "type": "boolean" }
                } } }
            }
        });
        let mut missing = Vec::new();
        properties_without_example("S", &schema, &mut missing);
        assert_eq!(
            missing,
            [
                "S.list",
                "S.list[].deeper",
                "S.map",
                "S.map.*.value",
                "S.oneOf[0].code",
                "S.oneOf[1].allOf[1].extra",
            ]
        );
    }

    /// Every response outside the 2xx range shows the body it carries: the
    /// route's own example, or the one [`add_middleware_error_examples`] and
    /// [`add_extractor_rejections`] write for a status the daemon's layers
    /// answer.
    #[test]
    fn error_responses_carry_an_example() {
        let doc = document_value();
        let mut missing = Vec::new();
        for (method, path, op) in operations(&doc) {
            for (status, response) in op["responses"].as_object().into_iter().flatten() {
                if status.starts_with('2') {
                    continue;
                }
                for (media, content) in response["content"].as_object().into_iter().flatten() {
                    let shown = content.get("example").is_some()
                        || content["examples"]
                            .as_object()
                            .is_some_and(|examples| !examples.is_empty());
                    if !shown {
                        missing.push(format!("{method} {path} {status} {media}"));
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "error responses without an example: {missing:?}"
        );
    }

    /// Escapes `token` for use in a JSON pointer (RFC 6901).
    fn pointer_token(token: &str) -> String {
        token.replace('~', "~0").replace('/', "~1")
    }

    /// One example to validate: the pointer of the subschema it must
    /// satisfy, where it was found (the same pointer for a schema's own
    /// example, else a body's `example` or `examples/<name>`), and its
    /// value.
    #[derive(Debug, Clone, PartialEq)]
    struct Example {
        /// The subschema the value must satisfy, as a pointer the
        /// validator map knows.
        pointer: String,
        /// Where the value was found, named in a violation when it is
        /// not the pointer itself.
        source: String,
        /// The example value.
        value: serde_json::Value,
    }

    impl Example {
        /// An example found on the schema at `pointer`.
        fn at(pointer: &str, value: &serde_json::Value) -> Self {
            Self {
                pointer: pointer.to_string(),
                source: pointer.to_string(),
                value: value.clone(),
            }
        }
    }

    /// Collects every example under `schema`, where the pointer locates
    /// the subschema the example belongs to: the schema's own `example`
    /// and `examples`, then those of its properties, the branches of
    /// `oneOf`, `allOf` and `anyOf`, array items and map values.
    fn examples_under(pointer: &str, schema: &serde_json::Value, out: &mut Vec<Example>) {
        if let Some(example) = schema.get("example") {
            out.push(Example::at(pointer, example));
        }
        for example in schema["examples"].as_array().into_iter().flatten() {
            out.push(Example::at(pointer, example));
        }
        for (name, property) in schema["properties"].as_object().into_iter().flatten() {
            let at = format!("{pointer}/properties/{}", pointer_token(name));
            examples_under(&at, property, out);
        }
        for key in ["oneOf", "allOf", "anyOf"] {
            for (index, branch) in schema[key].as_array().into_iter().flatten().enumerate() {
                examples_under(&format!("{pointer}/{key}/{index}"), branch, out);
            }
        }
        if schema["items"].is_object() {
            examples_under(&format!("{pointer}/items"), &schema["items"], out);
        }
        if schema["additionalProperties"].is_object() {
            examples_under(
                &format!("{pointer}/additionalProperties"),
                &schema["additionalProperties"],
                out,
            );
        }
    }

    /// Compiles `schema` as a JSON Schema 2020-12 root, with every
    /// reachable subschema addressable by pointer and formats asserted,
    /// not only annotated. With `components`, the root carries the
    /// document's component schemas under `$defs`, so references to them
    /// resolve inside the root and each is addressable as `#/$defs/<name>`
    /// (the layer in [`crate::openapi_contract`] compiles the same way).
    fn validators(
        components: Option<&serde_json::Value>,
        schema: &serde_json::Value,
    ) -> jsonschema::ValidatorMap {
        match components {
            Some(components) => crate::openapi_contract::validators(components, schema),
            None => jsonschema::options()
                .with_draft(jsonschema::Draft::Draft202012)
                .should_validate_formats(true)
                .build_map(schema)
                .expect("the document's schemas are valid JSON Schema"),
        }
    }

    /// Collects a body's own examples from `content`, a Media Type
    /// Object: its `example` and the `value` of each entry in its
    /// `examples`, each against the body schema at `pointer`; an entry
    /// without a value is a violation under `at`.
    fn body_examples(
        at: &str,
        pointer: &str,
        content: &serde_json::Value,
        out: &mut Vec<Example>,
        violations: &mut Vec<String>,
    ) {
        if let Some(example) = content.get("example") {
            out.push(Example {
                pointer: pointer.to_string(),
                source: "example".to_string(),
                value: example.clone(),
            });
        }
        for (name, entry) in content["examples"].as_object().into_iter().flatten() {
            let Some(value) = entry.get("value") else {
                violations.push(format!("{at} examples/{name}: no value"));
                continue;
            };
            out.push(Example {
                pointer: pointer.to_string(),
                source: format!("examples/{name}"),
                value: value.clone(),
            });
        }
    }

    /// Validates each example against the validator at its pointer,
    /// recording every violation under `at`, with the example's source
    /// when it is not the pointer itself.
    fn check_examples(
        at: &str,
        map: &jsonschema::ValidatorMap,
        examples: &[Example],
        out: &mut Vec<String>,
    ) {
        for example in examples {
            let label = if example.source == example.pointer {
                format!("{at} {}", example.pointer)
            } else {
                format!("{at} {} {}", example.source, example.pointer)
            };
            let Some(validator) = map.get(&example.pointer) else {
                out.push(format!("{label}: no schema compiled at this pointer"));
                continue;
            };
            for error in validator.iter_errors(&example.value) {
                out.push(format!(
                    "{label}: {error} (at instance path {})",
                    error.instance_path()
                ));
            }
        }
    }

    /// Every example in the document is valid against the schema it
    /// illustrates: a schema's own `example` and `examples` and those of
    /// its properties, nested objects, composition branches, array items
    /// and map values, in the components and inline in a body, plus each
    /// body's `example` and the `value` of each named entry in its
    /// `examples`. The check follows `$ref`s and asserts `format`s, so an
    /// example that omits a field the type requires, carries a value of
    /// the wrong type, uses a value outside an `enum`, or breaks a `uuid`
    /// or `date-time` format fails here instead of in review.
    #[test]
    fn every_example_matches_its_schema() {
        let doc = document_value();
        let components = &doc["components"]["schemas"];
        let mut violations = Vec::new();
        let mut checked = 0;

        // The components, compiled once: every pointer under `#/$defs`.
        let mut examples = Vec::new();
        for (name, schema) in components.as_object().into_iter().flatten() {
            examples_under(
                &format!("#/$defs/{}", pointer_token(name)),
                schema,
                &mut examples,
            );
        }
        let component_map = validators(Some(components), &serde_json::json!({}));
        check_examples("components", &component_map, &examples, &mut violations);
        checked += examples.len();

        // Each body: the content's own examples against the content schema,
        // and the examples inside an inline schema against their pointers.
        // A body that is one reference to a component reuses the component
        // validators, since its nested examples were checked above.
        for (method, path, op) in operations(&doc) {
            let bodies = std::iter::once(("request", &op["requestBody"])).chain(
                op["responses"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(s, r)| (s.as_str(), r)),
            );
            for (status, body) in bodies {
                for (media, content) in body["content"].as_object().into_iter().flatten() {
                    let Some(schema) = content.get("schema") else {
                        continue;
                    };
                    let at = format!("{method} {path} {status} {media}");
                    let mut examples = Vec::new();
                    let referenced = schema["$ref"]
                        .as_str()
                        .filter(|_| schema.as_object().is_some_and(|map| map.len() == 1))
                        .and_then(|target| target.strip_prefix("#/components/schemas/"));
                    if let Some(name) = referenced {
                        let pointer = format!("#/$defs/{}", pointer_token(name));
                        body_examples(&at, &pointer, content, &mut examples, &mut violations);
                        check_examples(&at, &component_map, &examples, &mut violations);
                        checked += examples.len();
                        continue;
                    }
                    body_examples(&at, "#", content, &mut examples, &mut violations);
                    examples_under("#", schema, &mut examples);
                    if examples.is_empty() {
                        continue;
                    }
                    let mut referenced = Vec::new();
                    refs(schema, &mut referenced);
                    let map = validators((!referenced.is_empty()).then_some(components), schema);
                    check_examples(&at, &map, &examples, &mut violations);
                    checked += examples.len();
                }
            }
        }
        assert!(
            violations.is_empty(),
            "{} example(s) do not match their schema:\n{}",
            violations.len(),
            violations.join("\n")
        );
        // Over two thousand today; a walk that finds far fewer has stopped
        // reaching the examples rather than found a smaller document.
        assert!(checked >= 2000, "only {checked} examples were checked");
    }

    /// The example check reaches an example on a property, inside a
    /// composition branch, on array items and on map values, follows a
    /// component reference (and asserts its format through it), reads a
    /// body's own `example` and named `examples`, and names each
    /// violation by its source, pointer and instance path.
    #[test]
    fn example_violations_are_named() {
        let components = serde_json::json!({
            "Id": { "type": "string", "format": "uuid", "example": "not-a-uuid" }
        });
        let schema = serde_json::json!({
            "type": "object",
            "required": ["id"],
            "example": { "id": "6d2f0c1e-8a4b-4f3d-9e7c-5b1a2c3d4e5f", "count": "three" },
            "examples": [ { "id": "6d2f0c1e-8a4b-4f3d-9e7c-5b1a2c3d4e5f" } ],
            "properties": {
                "id": { "$ref": "#/components/schemas/Id" },
                "count": { "type": "integer", "example": 3 },
                "mode": { "oneOf": [
                    { "type": "string", "enum": ["deny", "outbound"], "example": "allow" }
                ] },
                "tags": { "type": "array", "items": { "type": "string", "example": 7 } },
                "limits": { "type": "object", "additionalProperties": {
                    "type": "integer", "minimum": 0, "example": -1
                } }
            }
        });
        let mut examples = Vec::new();
        examples_under("#", &schema, &mut examples);
        let map = validators(Some(&components), &schema);
        let mut violations = Vec::new();
        check_examples("S", &map, &examples, &mut violations);
        let pointers: Vec<&str> = violations
            .iter()
            .map(|v| v.split(':').next().unwrap_or(v))
            .collect();
        assert_eq!(
            pointers,
            [
                "S #",
                "S #/properties/limits/additionalProperties",
                "S #/properties/mode/oneOf/0",
                "S #/properties/tags/items",
            ]
        );
        assert!(
            violations[0].contains("/count"),
            "the root example's wrong field is named: {}",
            violations[0]
        );
        let mut examples = Vec::new();
        examples_under("#/$defs/Id", &components["Id"], &mut examples);
        let map = validators(Some(&components), &serde_json::json!({}));
        let mut violations = Vec::new();
        check_examples("components", &map, &examples, &mut violations);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].starts_with("components #/$defs/Id:"));
        let mut violations = Vec::new();
        check_examples(
            "S",
            &map,
            &[Example::at("#/nowhere", &serde_json::json!(1))],
            &mut violations,
        );
        assert_eq!(
            violations,
            ["S #/nowhere: no schema compiled at this pointer"]
        );

        // A body: its `example` and named `examples` are checked against
        // the body schema, a referenced format included, and a named
        // example is reported by its name; an entry without a value too.
        let content = serde_json::json!({
            "schema": { "$ref": "#/components/schemas/Id" },
            "example": "6d2f0c1e-8a4b-4f3d-9e7c-5b1a2c3d4e5f",
            "examples": {
                "route": { "summary": "ok", "value": "not-a-uuid" },
                "bare": { "summary": "no value" }
            }
        });
        let mut examples = Vec::new();
        let mut violations = Vec::new();
        body_examples("B", "#/$defs/Id", &content, &mut examples, &mut violations);
        assert_eq!(violations, ["B examples/bare: no value"]);
        assert_eq!(
            examples
                .iter()
                .map(|e| e.source.as_str())
                .collect::<Vec<_>>(),
            ["example", "examples/route"]
        );
        check_examples("B", &map, &examples, &mut violations);
        assert_eq!(violations.len(), 2, "{violations:?}");
        assert!(
            violations[1].starts_with("B examples/route #/$defs/Id: "),
            "{}",
            violations[1]
        );
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "id": { "$ref": "#/components/schemas/Id" } },
            "example": { "id": "not-a-uuid" }
        });
        let mut examples = Vec::new();
        examples_under("#", &schema, &mut examples);
        let map = validators(Some(&components), &schema);
        let mut violations = Vec::new();
        check_examples("S", &map, &examples, &mut violations);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].contains("/id"),
            "the referenced format is asserted through the reference: {}",
            violations[0]
        );
    }

    /// Every route that reads a JSON body states the extractor's refusals
    /// as `text/plain` responses with the text axum writes, except the
    /// routes that read their body themselves, and a route's own words for
    /// a status are kept beside them.
    #[test]
    fn json_body_routes_state_extractor_rejections() {
        let doc = document_value();
        let mut missing = Vec::new();
        for (method, path, op) in operations(&doc) {
            let reads_json = op["requestBody"]["content"]
                .as_object()
                .is_some_and(|content| content.keys().any(|m| m.starts_with("application/json")));
            if !reads_json || MANUAL_JSON_BODIES.contains(&path.as_str()) {
                continue;
            }
            let optional = op["requestBody"]["required"] != serde_json::json!(true);
            for (status, _, _) in EXTRACTOR_REJECTIONS {
                if status == "415" && optional {
                    assert!(
                        op["responses"].get(status).is_none(),
                        "{method} {path} states a 415 its optional body never produces"
                    );
                    continue;
                }
                let text = &op["responses"][status]["content"]["text/plain"];
                let stated = text["example"].is_string()
                    || text["examples"]["extractor"]["value"].is_string();
                if !stated {
                    missing.push(format!("{method} {path} {status}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "extractor refusals not stated: {missing:?}"
        );

        let config = &doc["paths"]["/api/config/plugin-instances/{id}/config"]["put"]["responses"];
        assert_eq!(
            config["422"]["content"]["text/plain"]["example"],
            serde_json::json!(
                "Failed to deserialize the JSON body into the target type: missing field `expected_revision` at line 1 column 2"
            )
        );
        assert_eq!(
            config["415"]["content"]["text/plain"]["example"],
            serde_json::json!("Expected request with `Content-Type: application/json`")
        );
        let bootstrap = &doc["paths"]["/api/auth/bootstrap"]["post"]["responses"];
        assert_eq!(
            bootstrap["422"]["content"]["text/plain"]["example"],
            serde_json::json!(
                "Failed to deserialize the JSON body into the target type: display_name: invalid type: integer `1`, expected a string at line 1 column 17"
            )
        );
        let scan = &doc["paths"]["/api/library/scan"]["post"]["responses"];
        assert!(scan.get("415").is_none(), "an optional body has no 415");
        assert!(scan["422"]["content"]["text/plain"]["example"].is_string());
        let preferences = &doc["paths"]["/api/preferences"]["patch"]["responses"];
        assert_eq!(
            preferences["400"]["content"]["text/plain"]["example"],
            serde_json::json!(
                "Failed to parse the request body as JSON: expected value at line 1 column 10"
            ),
            "a route's own extractor text is kept as it is"
        );
        assert!(preferences["400"]["content"]["text/plain"]
            .get("examples")
            .is_none());
        let hints = &doc["paths"]["/api/discover/purchase-hints"]["post"]["responses"]["400"];
        let hints_text = &hints["content"]["text/plain"];
        assert!(hints_text.get("example").is_none());
        assert_eq!(
            hints_text["examples"]["route"]["value"],
            serde_json::json!("title, asin, isbn, or candidate_product_id is required"),
            "a route's own text moves to a named example"
        );
        assert_eq!(
            hints_text["examples"]["extractor"]["value"],
            serde_json::json!(
                "Failed to parse the request body as JSON: expected value at line 1 column 1"
            )
        );
        assert!(hints["description"]
            .as_str()
            .is_some_and(|d| d.ends_with("when the body is not syntactically valid JSON.")));
        let owner = &doc["paths"]["/api/config/nodes/{node_id}/owner"]["put"]["responses"];
        assert_eq!(
            owner["422"]["content"]["text/plain"]["example"],
            serde_json::json!(
                "Failed to deserialize the JSON body into the target type: invalid type: null, expected struct PutNodeOwner at line 1 column 4"
            ),
            "a body with only optional fields of other kinds names its struct"
        );
        let users = &doc["paths"]["/api/users"]["post"]["responses"]["400"];
        assert!(
            users["content"]["application/json"].is_object()
                && users["content"]["text/plain"]["example"].is_string(),
            "a route's own JSON body keeps its place beside the extractor's text"
        );
    }

    /// The 422 text names the first required field, then the first string,
    /// boolean or array field, and falls back when the body is not an object.
    #[test]
    fn extractor_rejection_text_follows_the_body() {
        use utoipa::openapi::schema::{ArrayBuilder, ObjectBuilder, Type};
        let text = |name: Option<&str>, object: Option<&utoipa::openapi::schema::Object>| {
            let body = object.map(|object| BodyObject { name, object });
            extractor_rejection_text("422", body.as_ref())
                .trim_start_matches("Failed to deserialize the JSON body into the target type: ")
                .to_string()
        };
        assert_eq!(
            text(None, None),
            "invalid type: null, expected a map at line 1 column 4"
        );
        let required = ObjectBuilder::new()
            .property("name", ObjectBuilder::new().schema_type(Type::String))
            .required("name")
            .build();
        assert_eq!(
            text(Some("Body"), Some(&required)),
            "missing field `name` at line 1 column 2"
        );
        let flag = ObjectBuilder::new()
            .property("on", ObjectBuilder::new().schema_type(Type::Boolean))
            .build();
        assert_eq!(
            text(None, Some(&flag)),
            "on: invalid type: integer `1`, expected a boolean at line 1 column 7"
        );
        let list = ObjectBuilder::new()
            .property(
                "ids",
                ArrayBuilder::new().items(ObjectBuilder::new().schema_type(Type::Integer)),
            )
            .build();
        assert_eq!(
            text(None, Some(&list)),
            "ids: invalid type: integer `1`, expected a sequence at line 1 column 8"
        );
        let numbers = ObjectBuilder::new()
            .property("n", ObjectBuilder::new().schema_type(Type::Integer))
            .build();
        assert_eq!(
            text(Some("Numbers"), Some(&numbers)),
            "invalid type: null, expected struct Numbers at line 1 column 4"
        );
        assert_eq!(
            text(None, Some(&numbers)),
            "invalid type: null, expected a map at line 1 column 4"
        );
    }

    /// Content that already holds the extractor's refusal, under any name,
    /// or an example named `extractor` is left as it is.
    #[test]
    fn extractor_example_is_added_once() {
        use utoipa::openapi::example::ExampleBuilder;
        use utoipa::openapi::{ContentBuilder, RefOr};
        let named = |name: &str, value: &str| {
            let mut content = ContentBuilder::new().build();
            content.examples.insert(
                name.to_string(),
                RefOr::T(ExampleBuilder::new().value(Some(value.into())).build()),
            );
            content
        };
        let mut by_value = named("not_json", "Failed to parse the request body as JSON: x");
        assert!(!add_extractor_example(
            &mut by_value,
            "Failed to parse the request body as JSON: y"
        ));
        assert_eq!(by_value.examples.len(), 1);
        let mut by_name = named("extractor", "some other text");
        assert!(!add_extractor_example(
            &mut by_name,
            "Failed to parse the request body as JSON: y"
        ));
        assert_eq!(by_name.examples.len(), 1);
    }

    /// A route's own 403 example is kept under its `error` slug when that is
    /// a slug, and as `route` when it is a sentence.
    #[test]
    fn enrollment_refusal_names_the_routes_own_example() {
        use utoipa::openapi::path::OperationBuilder;
        use utoipa::openapi::{ContentBuilder, Ref, ResponseBuilder, ResponsesBuilder};
        let names = |error: &str| {
            let content = ContentBuilder::new()
                .schema(Some(Ref::from_schema_name("ErrorJson")))
                .example(Some(serde_json::json!({ "error": error })))
                .build();
            let response = ResponseBuilder::new()
                .description("Forbidden.")
                .content("application/json", content)
                .build();
            let mut op = OperationBuilder::new()
                .responses(ResponsesBuilder::new().response("403", response).build())
                .build();
            add_enrollment_refusal(&mut op, false);
            let op = serde_json::to_value(&op).unwrap();
            let mut names: Vec<String> = op["responses"]["403"]["content"]["application/json"]
                ["examples"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            names.sort();
            names
        };
        assert_eq!(
            names("not_owner2"),
            ["mfa_enrollment_required", "not_owner2"]
        );
        assert_eq!(
            names("Only the owner may do that."),
            ["mfa_enrollment_required", "route"]
        );
        assert_eq!(names(""), ["mfa_enrollment_required", "route"]);
    }

    /// A route's own `text/plain` example moves beside the extractor's as a
    /// named example, and one that already is the extractor's stays alone.
    #[test]
    fn extractor_example_keeps_the_routes_text() {
        use utoipa::openapi::ContentBuilder;
        let mut own = ContentBuilder::new()
            .example(Some("title is required".into()))
            .build();
        assert!(add_extractor_example(
            &mut own,
            "Failed to parse the request body as JSON: x"
        ));
        assert!(own.example.is_none());
        assert_eq!(own.examples.len(), 2);
        assert!(!add_extractor_example(
            &mut own,
            "Failed to parse the request body as JSON: x"
        ));
        let mut already = ContentBuilder::new()
            .example(Some(
                "Expected request with `Content-Type: application/json`".into(),
            ))
            .build();
        assert!(!add_extractor_example(
            &mut already,
            "Expected request with `Content-Type: application/json`"
        ));
        assert!(already.example.is_some() && already.examples.is_empty());
        let mut bare = ContentBuilder::new().build();
        assert!(add_extractor_example(
            &mut bare,
            "Failed to parse the request body as JSON: x"
        ));
        assert_eq!(bare.examples.len(), 1);
    }

    /// Every error response has a body: its own, or the `ErrorJson` the
    /// error middleware writes for a bare status.
    #[test]
    fn error_responses_have_a_body() {
        let doc = document_value();
        for (method, path, op) in operations(&doc) {
            for (status, response) in op["responses"].as_object().into_iter().flatten() {
                if status.starts_with('4') || status.starts_with('5') {
                    assert!(
                        response["content"]
                            .as_object()
                            .is_some_and(|c| !c.is_empty()),
                        "{method} {path} {status} has no body"
                    );
                }
            }
        }
    }

    /// An error response the middleware answers shows the body it writes
    /// for that status, and a route's own example is kept.
    #[test]
    fn middleware_error_responses_carry_their_example() {
        let doc = document_value();
        let users = &doc["paths"]["/api/users"]["get"]["responses"];
        assert_eq!(
            users["401"]["content"]["application/json"]["example"],
            serde_json::json!({
                "error": "unauthorized",
                "message": "Operator authentication is required. Sign in with your operator token.",
                "status": 401
            })
        );
        assert_eq!(
            users["504"]["content"]["text/plain"]["example"],
            serde_json::json!("request timed out")
        );
        let logo = &doc["paths"]["/api/plugins/{kind}/{id}/logo"]["get"]["responses"]["404"];
        assert_eq!(
            logo["content"]["application/json"]["example"]["error"],
            serde_json::json!("not_found")
        );
        let patch = &doc["paths"]["/api/users/{id}"]["patch"]["responses"];
        assert_eq!(
            patch["409"]["content"]["application/json"]["example"],
            serde_json::json!({ "error": "last_owner" })
        );
    }

    /// A handler that reads a JSON body declares it, so generated clients
    /// can send it.
    #[test]
    fn json_bodies_are_declared() {
        let roots = [
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../bookclerk-integrations/src/portal"),
        ];
        let mut missing = Vec::new();
        for root in roots {
            let mut stack = vec![root];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir).expect("read src") {
                    let path = entry.expect("dir entry").path();
                    if path.is_dir() {
                        stack.push(path);
                        continue;
                    }
                    if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                        continue;
                    }
                    let text =
                        without_test_items(&std::fs::read_to_string(&path).expect("read source"));
                    for (at, _) in text.match_indices("#[utoipa::path(") {
                        let rest = &text[at..];
                        let Some(attr_end) = rest.find("\n)]") else {
                            continue;
                        };
                        let attr = &rest[..attr_end];
                        let Some(fn_at) = rest.find("fn ") else {
                            continue;
                        };
                        let sig_end = rest[fn_at..].find('{').map_or(rest.len(), |i| fn_at + i);
                        let signature = &rest[fn_at..sig_end];
                        let reads_json =
                            signature.contains(": Json<") || signature.contains(": Option<Json<");
                        if reads_json && !attr.contains("request_body") {
                            let name = signature[3..]
                                .split(['(', '<'])
                                .next()
                                .unwrap_or("")
                                .to_string();
                            missing.push(format!("{}: {name}", path.display()));
                        }
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "handlers read a JSON body their #[utoipa::path] does not declare: {missing:?}"
        );
    }

    /// The live socket is documented at the path its re-auth checks use.
    #[test]
    fn the_live_socket_path_matches_its_constant() {
        assert!(document_value()["paths"][crate::live::LIVE_PATH]["get"].is_object());
    }

    /// Every operation has a tag and an operation id no other one shares.
    #[test]
    fn every_operation_is_tagged_and_uniquely_named() {
        let doc = document_value();
        let mut ids = BTreeSet::new();
        for (method, path, op) in operations(&doc) {
            assert!(
                op["tags"].as_array().is_some_and(|t| t.len() == 1),
                "{method} {path} has no tag"
            );
            let id = op["operationId"].as_str().unwrap_or_default().to_string();
            assert!(!id.is_empty(), "{method} {path} has no operation id");
            assert!(ids.insert(id.clone()), "operation id {id} is used twice");
        }
    }

    /// Every schema an operation references is in the components.
    #[test]
    fn every_schema_reference_resolves() {
        let doc = document_value();
        let schemas = doc["components"]["schemas"]
            .as_object()
            .expect("components.schemas");
        let mut found = Vec::new();
        refs(&doc, &mut found);
        assert!(!found.is_empty());
        for target in found {
            let name = target
                .strip_prefix("#/components/schemas/")
                .unwrap_or_else(|| panic!("unexpected $ref {target}"));
            assert!(schemas.contains_key(name), "{target} is not defined");
        }
    }

    /// Each operation states the access its group enforces: public routes
    /// require nothing, guarded ones list the schemes that pass the layer.
    #[test]
    fn operations_state_their_access() {
        let doc = document_value();
        let find = |method: &str, path: &str| doc["paths"][path][method].clone();
        let health = find("get", "/health");
        assert_eq!(health["security"], serde_json::json!([]));
        let reload = find("post", "/api/config/reload");
        assert_eq!(
            reload["security"],
            serde_json::json!([{ OPERATOR_TOKEN: [] }, { OPERATOR_SESSION: [] }])
        );
        assert_eq!(
            reload["x-bookclerk-access"],
            serde_json::json!(Access::Operator.rule())
        );
        let books = find("get", "/api/library/books");
        assert!(books["security"]
            .as_array()
            .is_some_and(|s| s.contains(&serde_json::json!({ PORTAL_SESSION: [] }))));
        // The Accounts routes are nested under their mount point.
        assert!(doc["paths"]["/api/portal/me"]["get"].is_object());
        assert_eq!(
            find("get", "/status")["deprecated"],
            serde_json::json!(true)
        );
    }

    /// Drops every `#[cfg(test)]` item from Rust source: the attribute and
    /// the item after it, up to its `;` or its balanced `{ … }`.
    ///
    /// # Arguments
    ///
    /// * `text` - A source file.
    ///
    /// # Returns
    ///
    /// The source without test items. Braces inside strings are counted
    /// too, which is close enough for the route scan.
    fn without_test_items(text: &str) -> String {
        const ATTR: &str = "#[cfg(test)]";
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find(ATTR) {
            out.push_str(&rest[..at]);
            let item = &rest[at + ATTR.len()..];
            let semi = item.find(';');
            let brace = item.find('{');
            let end = match (semi, brace) {
                (Some(semi), Some(brace)) if semi < brace => semi + 1,
                (_, Some(brace)) => {
                    let mut depth = 0usize;
                    let mut end = item.len();
                    for (i, c) in item[brace..].char_indices() {
                        match c {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    end = brace + i + 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    end
                }
                (Some(semi), None) => semi + 1,
                (None, None) => item.len(),
            };
            rest = &item[end..];
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn test_items_are_dropped_and_the_rest_kept() {
        let text = "a\n#[cfg(test)]\nmod t;\nb\n#[cfg(test)]\nmod u { fn f() { x } }\nc\n";
        assert_eq!(without_test_items(text), "a\n\nb\n\nc\n");
    }

    /// The only routes registered outside an `OpenApiRouter` are the ones
    /// [`UNDOCUMENTED_ROUTES`] lists, so a new endpoint cannot skip the
    /// document. Test modules and `*_tests.rs` files are not scanned.
    #[test]
    fn only_listed_routes_bypass_the_spec() {
        let roots = [
            Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf(),
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../bookclerk-integrations"),
        ];
        let mut seen = Vec::new();
        for root in roots {
            let mut stack = vec![root.join("src")];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir).expect("read src") {
                    let path = entry.expect("dir entry").path();
                    if path.is_dir() {
                        stack.push(path);
                        continue;
                    }
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if !name.ends_with(".rs") || name.ends_with("tests.rs") {
                        continue;
                    }
                    let text = std::fs::read_to_string(&path).expect("read source");
                    let code = without_test_items(&text);
                    let rel = path
                        .strip_prefix(&root)
                        .expect("under root")
                        .to_string_lossy()
                        .replace('\\', "/");
                    for marker in [
                        ".route(",
                        ".route_service(",
                        ".fallback_service(",
                        ".nest_service(",
                    ] {
                        for (at, _) in code.match_indices(marker) {
                            // Only router calls: `.route(` followed by a
                            // path literal or a `path` binding, not a
                            // registry's `.route(&key, …)`.
                            let rest = &code[at..];
                            let end = rest.find('\n').unwrap_or(rest.len());
                            let line = rest[..end].trim_end().trim_end_matches(';');
                            let arg = &rest[marker.len()..];
                            if marker == ".route(" && arg.starts_with('&') {
                                continue;
                            }
                            if line.starts_with(".route(…)") {
                                continue;
                            }
                            seen.push((rel.clone(), line.to_string()));
                        }
                    }
                }
            }
        }
        for (file, line) in &seen {
            assert!(
                UNDOCUMENTED_ROUTES
                    .iter()
                    .any(|(f, snippet)| f == file && line.starts_with(snippet)),
                "{file}: `{line}` registers a route outside the OpenAPI document; \
                 register it with `.routes(routes!(handler))` and a `#[utoipa::path]`"
            );
        }
        assert_eq!(seen.len(), UNDOCUMENTED_ROUTES.len(), "{seen:?}");
    }

    /// The daemon serves the document without auth, and it is the one
    /// [`document`] builds.
    #[tokio::test]
    async fn the_router_serves_the_document() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let _dek = crate::auth::tests::process_dek_lock().await;
        let (_state, app, _library) =
            crate::auth::tests::phase2_harness_unseeded("op-token-openapi").await;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(OPENAPI_PATH)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let served: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
        assert_eq!(served, document_value());

        let response = app
            .oneshot(
                Request::builder()
                    .uri(v30::OPENAPI_V30_PATH)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let served: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
        let expected: serde_json::Value =
            serde_json::from_str(&document_v30_json().expect("restates")).expect("JSON");
        assert_eq!(served, expected);
    }
}
