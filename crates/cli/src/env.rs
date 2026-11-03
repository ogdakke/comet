//! The edge and sign-in client `zeron` resolves from its environment.

/// Production edge (Cloudflare Worker + Durable Objects on the zeron.sh zone).
/// `ZERON_EDGE_URL` overrides (local dev / self-hosting).
pub const DEFAULT_EDGE_URL: &str = "https://edge.zeron.sh";

/// Production WorkOS AuthKit client id — public knowledge (it appears in every
/// authorize URL), so baking it in is safe. Overridden by `ZERON_WORKOS_CLIENT_ID`;
/// set it to the empty string — or set a dev bearer via `ZERON_EDGE_TOKEN` — to
/// force dev-mode auth instead.
pub const DEFAULT_WORKOS_CLIENT_ID: &str = "client_01KWD0EAKZKD50YCQJNYSRE4BY";

pub fn edge_url_from_env() -> String {
    std::env::var("ZERON_EDGE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_default()
}

/// WorkOS requires an explicit client id (`ZERON_WORKOS_CLIENT_ID`, also read
/// from the private deployment `env` file). An explicit development bearer
/// still enables the local development edge flow; no configuration remains
/// local-only. (Fork: no baked production edge or client id.)
pub fn workos_client_id_from_env(edge_token: &Option<String>) -> Option<String> {
    match std::env::var("ZERON_WORKOS_CLIENT_ID") {
        Ok(v) if v.trim().is_empty() => None,
        Ok(v) => Some(v),
        Err(_) if edge_token.is_some() => None,
        Err(_) => None,
    }
}

/// The engine's IPC port: `ZERON_IPC_PORT`, else the packaged port for an
/// installed build and an isolated one for a source build
/// ([`crate::paths::default_ipc_port`]).
pub fn ipc_port_from_env() -> u16 {
    std::env::var("ZERON_IPC_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(crate::paths::default_ipc_port)
}

/// [`ipc_port_from_env`] from the variable's value.
pub fn ipc_port_from(value: Option<&str>) -> u16 {
    value.and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_IPC_PORT)
}

/// The production engine's IPC port.
pub const DEFAULT_IPC_PORT: u16 = 27654;
