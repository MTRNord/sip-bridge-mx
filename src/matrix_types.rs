use ruma_common::events::macros::EventContent;
use serde::{Deserialize, Serialize};

/// The payload for our call event.
#[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
#[ruma_event(type = "org.matrix.msc3401.call", kind = State, state_key_type = String)]
pub struct CallStateContent {
    #[serde(rename = "io.element.ptt")]
    pub ptt: Option<bool>,
    #[serde(rename = "m.intent")]
    pub intent: String,
    #[serde(rename = "m.type")]
    pub call_type: String,
    #[serde(rename = "m.terminated")]
    pub terminated: Option<bool>,
    #[serde(rename = "m.name")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CallTrackSettings {
    #[serde(rename = "channelCount")]
    pub channel_count: Option<u16>,
    #[serde(rename = "sampelRate")]
    pub sampel_rate: Option<u64>,
    #[serde(rename = "m.maxbr")]
    pub maxbr: u64,
    pub width: Option<u64>,
    pub height: Option<u64>,
    #[serde(rename = "facingMode")]
    pub facing_mode: Option<String>,
    #[serde(rename = "frameRate")]
    pub frame_rate: Option<f32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CallTrack {
    pub kind: String,
    pub id: String,
    pub label: String,
    pub settings: CallTrackSettings,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CallFeed {
    pub purpose: String,
    pub id: String,
    pub tracks: Vec<CallTrack>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CallDevices {
    pub device_id: String,
    pub session_id: String,
    pub expires_ts: String,
    pub feeds: Vec<CallFeed>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JoinedCall {
    #[serde(rename = "m.call_id")]
    pub call_id: String,
    #[serde(rename = "m.devices")]
    pub devices: Vec<CallDevices>,
}

/// The payload for our call member event.
#[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
#[ruma_event(type = "org.matrix.msc3401.member", kind = State, state_key_type = String)]
pub struct CallMemberStateContent {
    #[serde(rename = "m.calls")]
    pub calls: Vec<JoinedCall>,
}
