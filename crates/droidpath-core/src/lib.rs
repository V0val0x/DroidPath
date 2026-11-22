#![allow(clippy::result_large_err)]

pub mod adb;
pub mod bridge;
pub mod manager;
pub mod mcp;
pub mod protocol;
pub mod router;
pub mod scrcpy;
pub mod shadow_dom;
pub mod spatial_graph;
pub mod transport;

pub use adb::{DroidPathActionExecutor, DroidPathAdbError, DroidPathAdbRunner, DroidPathDevice};
pub use bridge::{
    DaemonRecoveryHook, ExploreCandidate, ExploreOptions, ExploreOutcome, RouteExecuteOptions,
    ShadowGraphBridge, ShadowNavigationOutcome,
};
pub use manager::{
    DaemonDeployReport, DaemonLauncherHook, DroidPathGraphStats, DroidPathManager,
    DroidPathPersistenceCoordinator, PortForwarderHook,
};
pub use mcp::{
    DroidPathHttpServer, DroidPathMcpServerCore, DroidPathStdioTransport,
    DroidPathUITreeFallbackParser, McpContentBlock, McpJsonRpcError, McpJsonRpcRequest,
    McpJsonRpcResponse, McpToolAnnotations, McpToolDefinition, McpToolResult, ToolSchemaExporter,
    ToolSchemaFormat,
};
pub use protocol::{
    DaemonCommand, NdjsonWireCodec, ShadowCompactNode, ShadowPatchMask, ShadowTreeMatchMode,
    ShadowUINode, WireCodec, WireCodecError, WireDiffPayload, WireErrorPayload,
    WireHeartbeatPayload, WireHelloPayload, WireInputResultPayload, WireMessage, WireNodePatch,
    WirePongPayload, WireSecureScreenPayload, WireSnapshotPayload, WireVisionFramePayload,
};
pub use router::{
    AStarCostConfiguration, AStarPathfinder, AStarSearchResult, DismissCandidate,
    HazardousConfirmationRequest, MacroActionExecutor, MacroEventCallback,
    MacroExecutionConfiguration, MacroExecutionEngine, MacroExecutionEvent, MacroExecutionReport,
    MacroStepReport, PathDeviationError, UniversalDismissDetector,
};
pub use scrcpy::{
    parse_h264_sps_dimensions, ScrcpyBridge, ScrcpyJarInspection, ScrcpyMetaOption,
    ScrcpySessionInfo, ScrcpyStartConfig,
};
pub use shadow_dom::{
    BoundsRect, ResyncHandler, ShadowDOMUpdate, ShadowDomState, ShadowTreeIndex, SnapshotMetadata,
};
pub use spatial_graph::{
    AgenticFeedback, AgenticFeedbackBuilder, AttributedUIAction, ChangedNodeSummary,
    FeedbackCandidate, HazardCategory, HazardousActionAssessment, HazardousActionClassifier,
    RankedVertexMatch, ScreenDeltaSummary, ScreenLayoutHasher, ScreenSemanticFingerprint,
    ScreenStateVertex, ScreenTransitionEdge, SeedGraphCatalog, SeedMergeReport,
    SpatialGraphEngine, SpatialGraphSnapshot, TopologyChangedHandler, UIActionKind,
};
pub use transport::{
    BeforeReconnectHook, DaemonClientError, DaemonClientMetrics, DaemonConnectionState,
    DaemonEndpointConfiguration, DaemonTcpClient, LineFramer, LineFramerError,
};
