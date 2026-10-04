//! MCP database queries and authenticated daemon image access.

pub mod images;
pub mod query;
pub mod transport;

use std::path::PathBuf;

use item_core::store::{ObservationFilter, RecordedItemsFilter};
use query::{
    DIRECT_NOTE, Database, GetRequest, GetResponse, InventoryRequest, InventoryResponse,
    ListRequest, ListResponse, LocateRequest, LocateResponse, limit, text, tool_error,
};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    CallToolResult, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerInfoRequest {}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ServerInfoResponse {
    pub name: &'static str,
    pub version: &'static str,
    pub transport: &'static str,
    pub mode: &'static str,
}

#[derive(Clone)]
pub struct ItemMcpServer {
    database: Database,
    daemon: Option<images::DaemonClient>,
}

impl ItemMcpServer {
    pub fn new(database_path: PathBuf) -> Self {
        Self {
            database: Database::new(database_path),
            daemon: None,
        }
    }

    pub fn with_daemon(mut self, daemon: images::DaemonClient) -> Self {
        self.daemon = Some(daemon);
        self
    }
}

#[tool_router]
impl ItemMcpServer {
    #[tool(
        description = "Validate the Item Finders MCP connection. Returns server identity and database query mode, not database health. Image tools require an authenticated local daemon. get_camera_frame freezes an expiring immutable JPEG; it does not create an observation.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn server_info(&self, _: Parameters<ServerInfoRequest>) -> Json<ServerInfoResponse> {
        Json(ServerInfoResponse {
            name: "item-mcp",
            version: env!("CARGO_PKG_VERSION"),
            transport: "stdio",
            mode: "queries_and_camera_frames",
        })
    }

    #[tool(
        description = "List distinct item labels actually recorded in the database. Start here when the exact name is unknown, especially for non-English queries. Labels may include detector false positives; this is not a list of items currently visible. Counts are observation records, not frame hits.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_recorded_items(
        &self,
        Parameters(request): Parameters<InventoryRequest>,
    ) -> Result<Json<InventoryResponse>, CallToolResult> {
        let limit = limit(request.limit, 200)?;
        let filter = RecordedItemsFilter {
            camera: text(request.camera)?,
            zone: text(request.zone)?,
            limit,
        };
        let page = self
            .database
            .read(move |store| store.recorded_items(&filter))
            .await?;
        Ok(Json(InventoryResponse {
            items: page.items.into_iter().map(Into::into).collect(),
            truncated: page.truncated,
            limit,
            source: "database_observation_labels",
            note: DIRECT_NOTE,
        }))
    }

    #[tool(
        description = "List direct observation records, newest first. Label matches a literal substring; camera and zone are exact. since/until are inclusive RFC3339 bounds on last_seen. Results are historical sightings, not current presence or hypotheses.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_observations(
        &self,
        Parameters(request): Parameters<ListRequest>,
    ) -> Result<Json<ListResponse>, CallToolResult> {
        let filter = request.filter()?;
        let limit = filter.limit;
        let observations = self
            .database
            .read(move |store| store.observations_filtered(&filter))
            .await?;
        Ok(Json(ListResponse {
            observations: observations.into_iter().map(Into::into).collect(),
            limit,
            time_filter: "last_seen_inclusive",
            source: "database_observations",
            note: DIRECT_NOTE,
        }))
    }

    #[tool(
        description = "Locate an item by its last direct sighting. Prefer an explicit recorded label (discover with list_recorded_items); label overrides query. English questions use a keyword heuristic, not semantic translation. Empty matches are not proof an item is absent. No current-location guarantee, images, or hypotheses.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn locate_item(
        &self,
        Parameters(request): Parameters<LocateRequest>,
    ) -> Result<Json<LocateResponse>, CallToolResult> {
        let explicit = text(request.label)?;
        let query = text(request.query)?;
        let (keyword, label_source) = match (explicit, query) {
            (Some(label), _) => (label, "explicit_label"),
            (None, Some(query)) => (item_query::keyword_of(&query), "query_keyword_heuristic"),
            (None, None) => {
                return Err(tool_error(
                    "invalid_input",
                    "Provide a recorded label or a nonempty query.",
                ));
            }
        };
        let limit = limit(request.limit, 20)?;
        let filter = ObservationFilter {
            label: Some(keyword.clone()),
            camera: text(request.camera)?,
            zone: text(request.zone)?,
            limit,
            ..Default::default()
        };
        let observations = self
            .database
            .read(move |store| store.observations_filtered(&filter))
            .await?;
        Ok(Json(LocateResponse {
            keyword,
            label_source,
            direct: observations.into_iter().map(Into::into).collect(),
            limit,
            source: "database_observations",
            note: DIRECT_NOTE,
        }))
    }

    #[tool(
        description = "Read unannotated raw JPEG evidence from the authenticated local daemon. Provide observation_ids or opaque asset_ids and image_kind=raw. Returns image content with metadata and hash; never falls back to detector-annotated snapshots. Historical captured frames do not prove current presence. No visual inference or writes are performed.",
        output_schema = images::output_schema(),
        annotations(read_only_hint = true, destructive_hint = false, open_world_hint = false)
    )]
    async fn get_item_images(
        &self,
        Parameters(request): Parameters<images::ImagesRequest>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, CallToolResult> {
        images::validate(&request)?;
        let daemon = self.daemon.as_ref().ok_or_else(|| {
            tool_error(
                "daemon_unavailable",
                "Configure the local daemon endpoint and token to read raw images.",
            )
        })?;
        let deadline = daemon.deadline();
        tokio::select! {
            biased;
            _ = context.ct.cancelled() => Err(tool_error("cancelled", "The image request was cancelled.")),
            result = tokio::time::timeout_at(deadline, async {
                let database_id = self.database.read(|store| store.database_identity()).await?
                    .ok_or_else(|| tool_error("schema_unavailable", "The query database has no daemon asset source identity."))?;
                daemon.images_at(request, database_id, deadline).await
            }) => result.unwrap_or_else(|_| Err(images::timeout_error())),
        }
    }

    #[tool(
        description = "Freeze an immutable unannotated JPEG from an exact camera ID through the authenticated local daemon. max_age_ms limits age at freeze, wait_timeout_ms bounds waiting for an eligible frame, and max_bytes bounds JPEG size. Returns an expiring opaque asset, capture_time, age_ms and camera_state at freeze, and an image block. Retries may capture a newer frame. Reread the same asset with get_item_images. No observation or visual inference is created; a captured frame does not prove current presence.",
        output_schema = images::output_schema(),
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    async fn get_camera_frame(
        &self,
        Parameters(request): Parameters<images::FrameRequest>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, CallToolResult> {
        request.validate()?;
        let daemon = self.daemon.as_ref().ok_or_else(|| {
            tool_error(
                "daemon_unavailable",
                "Configure the local daemon endpoint and token to capture a camera frame.",
            )
        })?;
        let deadline = daemon.deadline();
        tokio::select! {
            biased;
            _ = context.ct.cancelled() => Err(tool_error("cancelled", "The camera frame request was cancelled.")),
            result = tokio::time::timeout_at(deadline, async {
                let database_id = self.database.read(|store| store.database_identity()).await?
                    .ok_or_else(|| tool_error("schema_unavailable", "The query database has no daemon asset source identity."))?;
                daemon.camera_frame_at(request, database_id, deadline).await
            }) => result.unwrap_or_else(|_| Err(images::timeout_error())),
        }
    }

    #[tool(
        description = "Get one direct observation by its positive observation_id. Returns basic label, camera, zone, first/last sighting time and hit count only, not image or geometry/event/depth details. Not found is reported as an error.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_observation(
        &self,
        Parameters(request): Parameters<GetRequest>,
    ) -> Result<Json<GetResponse>, CallToolResult> {
        if request.observation_id <= 0 {
            return Err(tool_error(
                "invalid_input",
                "observation_id must be positive.",
            ));
        }
        let observation = self
            .database
            .read(move |store| store.observation_by_id(request.observation_id))
            .await?
            .ok_or_else(|| tool_error("not_found", "No observation with this ID is recorded."))?;
        Ok(Json(GetResponse {
            observation: observation.into(),
            source: "database_observations",
            detail_level: "basic",
            note: DIRECT_NOTE,
        }))
    }
}

#[tool_handler]
impl ServerHandler for ItemMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("item-mcp", env!("CARGO_PKG_VERSION")))
            .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
            .with_instructions(
                "Database queries and camera frames. get_camera_frame freezes an expiring immutable JPEG through the authenticated daemon; retries may capture a newer frame. age_ms and camera_state describe freeze time, and capture_time identifies the historical frame. It does not create an observation. Reread the same opaque asset with the read-only get_item_images tool until expires_at. When an item name is uncertain, call list_recorded_items first, then locate_item with an explicit recorded label. list_observations and get_observation return historical direct sightings only: do not claim an item is currently there. Labels come from detectors/webhooks and may be wrong. get_item_images reads unannotated raw evidence through an authenticated local daemon. Use observation IDs returned by the query tools; raw may be unavailable for historical or Frigate-only observations. Image metadata identifies each content block and source hash. Do not treat your visual inference as a database observation. No hypotheses, camera controls, annotations or database writes are available. server_info confirms the connection, not database availability.",
            )
    }
}
