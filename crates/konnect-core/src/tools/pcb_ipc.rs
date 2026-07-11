//! Shared IPC-first / file-fallback plumbing for the `pcb_*` toolsets.
//!
//! Upstream PR #5 established the pattern: try the KiCAD IPC API first (live
//! board, undo-aware), fall back to editing the `.kicad_pcb` file when no IPC
//! transport exists, and tag every response with `source: "ipc" | "file"` so
//! callers know which board state answered.
//!
//! The safety rule enforced here: file fallback for WRITE tools is only
//! allowed when there is *no transport at all* ([`IpcAttempt::Unavailable`]).
//! If a live KiCAD session is reachable but the call failed
//! ([`IpcAttempt::Failed`] — bad request, no board open, wedged session),
//! write tools must refuse instead of editing the file behind the live GUI's
//! back — the GUI's eventual save would clobber the file edit (or vice
//! versa), which is exactly the split-brain bug this module exists to fix.

use crate::mcp::protocol::CallToolResult;
use crate::tools::ToolContext;
use konnect_ipc::client::KiCadIpcClient;

/// Outcome of attempting an operation over the KiCAD IPC API.
pub enum IpcAttempt<T> {
    /// The IPC call succeeded.
    Ok(T),
    /// No IPC transport exists (no socket configured / nothing listening).
    /// File fallback is safe: there is no live session to diverge from.
    Unavailable(String),
    /// A transport exists (live KiCAD answered or is at least present) but
    /// the call failed. Write tools must NOT fall back to file edits.
    Failed(String),
}

/// Run `f` against the IPC client on a blocking thread and classify the result.
pub async fn try_ipc<T, F>(ctx: &ToolContext, f: F) -> anyhow::Result<IpcAttempt<T>>
where
    T: Send + 'static,
    F: FnOnce(&KiCadIpcClient) -> anyhow::Result<T> + Send + 'static,
{
    let addr = ctx.config.ipc_address.clone();
    match tokio::task::spawn_blocking(move || f(&KiCadIpcClient::new(&addr))).await {
        Ok(Ok(v)) => Ok(IpcAttempt::Ok(v)),
        Ok(Err(e)) => {
            if konnect_ipc::client::is_unavailable(&e) {
                Ok(IpcAttempt::Unavailable(e.to_string()))
            } else {
                Ok(IpcAttempt::Failed(e.to_string()))
            }
        }
        Err(e) => Err(anyhow::anyhow!("Thread error: {}", e)),
    }
}

/// The consistency-guard refusal for write tools: a live IPC session exists
/// but the call failed for a non-transport reason. Editing the file now would
/// happen behind the live GUI's back, so refuse with an actionable error.
pub fn ipc_write_refused(tool: &str, msg: &str) -> CallToolResult {
    CallToolResult::error(format!(
        "{tool}: a KiCAD IPC session is reachable but the IPC call failed ({msg}). \
         Refusing to fall back to editing the .kicad_pcb file while a live KiCAD \
         session exists — the session's unsaved state and the file would diverge \
         and one would clobber the other. Fix the IPC-side problem (usually: open \
         the board in KiCAD's PCB editor), or close KiCAD entirely to work \
         file-based, then retry."
    ))
}
