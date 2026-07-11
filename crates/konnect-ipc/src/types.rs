use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcVector2 {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcFootprint {
    pub reference: String,
    pub value: String,
    pub footprint: String,
    pub position: IpcVector2,
    pub rotation: f64,
    pub layer: String,
}

/// A pad on a footprint. `position` is relative to the parent footprint's
/// origin (unrotated), matching both the IPC API and .kicad_pcb file
/// conventions — callers transform to board space with the footprint's
/// position and rotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcPad {
    pub number: String,
    pub position: IpcVector2,
    pub net: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcTitleBlock {
    pub title: String,
    pub date: String,
    pub revision: String,
    pub company: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcTrack {
    pub net_name: String,
    pub layer: String,
    pub width: f64,
    pub start: IpcVector2,
    pub end: IpcVector2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcNet {
    pub name: String,
    pub netcode: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcLayer {
    pub name: String,
    pub id: i32,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcBoardExtents {
    pub min: IpcVector2,
    pub max: IpcVector2,
}
