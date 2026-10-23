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
    ("Auth", "/api/auth", "Sign-in, sessions, passkeys, TOTP and the operator's elevation."),
    ("Users", "/api/users", "Library users and their claim tickets."),
    ("Library", "/api/library", "Books in the library, scans and acquires."),
    ("Discover", "/api/discover", "Recommendations, catalog search and title metadata."),
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
    for item in group.paths.paths.values_mut() {
        for op in operations_mut(item) {
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
    doc
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
        .property("error", ObjectBuilder::new().schema_type(Type::String).description(Some("Stable machine slug.")))
        .required("error")
        .property("message", ObjectBuilder::new().schema_type(Type::String).description(Some("Explanation for people.")))
        .property("status", ObjectBuilder::new().schema_type(Type::Integer).description(Some("The HTTP status, repeated.")))
        .build()
        .into()
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

/// The document, built once per process and shared by every router.
///
/// # Returns
///
/// The same [`document`] each call.
pub fn shared_document() -> Arc<OpenApi> {
    static DOCUMENT: std::sync::OnceLock<Arc<OpenApi>> = std::sync::OnceLock::new();
    DOCUMENT.get_or_init(|| Arc::new(document())).clone()
}

/// Serves the OpenAPI document.
#[utoipa::path(
    get,
    path = "/api/openapi.json",
    responses((status = 200, description = "The OpenAPI 3.1 document for this daemon's version.", content_type = "application/json")),
)]
pub async fn get_openapi(
    axum::extract::State(doc): axum::extract::State<Arc<OpenApi>>,
) -> Json<Arc<OpenApi>> {
    Json(doc)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    /// HTTP methods an OpenAPI path item can hold, in JSON key form.
    const METHODS: &[&str] = &[
        "get", "put", "post", "delete", "options", "head", "patch", "trace",
    ];

    /// Plain axum registrations that stay out of the document, as
    /// `file: snippet` pairs: the invite page and the SPA's static files.
    const UNDOCUMENTED_ROUTES: &[(&str, &str)] = &[
        ("src/api.rs", ".route(\"/invite\", get(invite_page))"),
        (
            "src/api.rs",
            ".route_service(path, ServeFile::new(index.clone()))",
        ),
        ("src/api.rs", ".fallback_service(ServeDir::new(dist))"),
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
    }
}
