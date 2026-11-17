//! `marshal-launch`: start an agent somewhere and see it join the roster.
//!
//! See `docs/agent-launcher.md`. The launcher owns the parts of a launch that
//! don't depend on where the agent runs (its id, its environment, waiting for
//! it to join, its first task). An [`Adapter`] does only the part that does:
//! run this command, with these environment variables, over there.
//!
//! This module fixes the shared types first, so the built-in tmux adapter and
//! the external `marshal-launch-<name>` adapters agree on them.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// The version of the adapter request/response contract this crate speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// What the launcher hands an adapter for `check` and `launch`.
///
/// Serialized as the JSON object an external adapter reads on stdin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchRequest {
    /// Contract version; [`PROTOCOL_VERSION`].
    pub version: u32,
    /// `claude` or `codex`.
    pub harness: String,
    /// The command to run, as given. Adapters add no flags to it.
    pub argv: Vec<String>,
    /// The folder to run it in, on the target.
    pub cwd: String,
    /// A label for the launch (tmux session name, VM name, ...).
    pub name: String,
    /// The `launch-<uuid>` id the agent's roster row will carry.
    pub agent_id: String,
    /// Variables to add to the agent's environment. Never secrets.
    pub env: BTreeMap<String, String>,
    /// Adapter-specific options, passed through after `--`.
    pub options: Vec<String>,
}

/// What an adapter reports after starting something.
///
/// Serialized as the JSON object an external adapter writes on stdout.
/// Unknown fields (such as the response's `version`) are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launched {
    /// Opaque to the launcher; the adapter's own way to find the launch again.
    pub handle: String,
    /// The host the agent runs on, as its roster row's `host.name` will show it.
    pub host: String,
    /// The folder the agent runs in, as its roster row's `cwd` will show it.
    pub cwd: String,
    /// Human-readable location, e.g. `build-03 tmux:claude-app`.
    pub r#where: String,
    /// A command that attaches a terminal to the agent, if there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach: Option<String>,
}

/// Something that can start an agent somewhere.
///
/// An adapter is never asked whether the agent joined; the roster answers that.
pub trait Adapter {
    /// The adapter's name, as in `marshal-launch <name> ...`.
    fn name(&self) -> &str;
    /// Confirm the target can run this request, starting nothing.
    fn check(&self, req: &LaunchRequest) -> Result<()>;
    /// Start the request's `argv` in its `cwd` with its `env` added.
    fn launch(&self, req: &LaunchRequest) -> Result<Launched>;
    /// Stop what `handle` names.
    fn stop(&self, handle: &str) -> Result<()>;
    /// Whether what `handle` names is still running; `None` if the adapter can't tell.
    fn status(&self, handle: &str) -> Result<Option<bool>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn launch_request_serializes_with_the_contract_names() {
        let req = LaunchRequest {
            version: PROTOCOL_VERSION,
            harness: "claude".into(),
            argv: vec!["claude".into()],
            cwd: "~/work/app".into(),
            name: "claude-app".into(),
            agent_id: "launch-6f1c".into(),
            env: BTreeMap::from([("MARSHAL_AGENT_ID".into(), "launch-6f1c".into())]),
            options: vec!["--host".into(), "build-03".into()],
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(
            v,
            json!({
                "version": 1,
                "harness": "claude",
                "argv": ["claude"],
                "cwd": "~/work/app",
                "name": "claude-app",
                "agent_id": "launch-6f1c",
                "env": {"MARSHAL_AGENT_ID": "launch-6f1c"},
                "options": ["--host", "build-03"],
            })
        );
        assert_eq!(serde_json::from_value::<LaunchRequest>(v).unwrap(), req);
    }

    #[test]
    fn launched_reads_an_adapter_response_and_names_where() {
        let l: Launched = serde_json::from_value(json!({
            "version": 1,
            "handle": "h1",
            "host": "build-03",
            "cwd": "/home/u/work/app",
            "where": "build-03 tmux:claude-app",
        }))
        .unwrap();
        assert_eq!(l.r#where, "build-03 tmux:claude-app");
        assert_eq!(l.attach, None);
        let v = serde_json::to_value(&l).unwrap();
        assert!(v.get("where").is_some());
        assert!(v.get("attach").is_none());
    }
}
