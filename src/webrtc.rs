use std::{fs::File, sync::Arc};

use matrix_sdk::{ruma::events::room::message::RoomMessageEventContent, Room};
use tokio::sync::{Mutex, Notify};
use tracing::{error, info};
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::{MediaEngine, MIME_TYPE_OPUS},
        APIBuilder,
    },
    ice_transport::{
        ice_candidate::RTCIceCandidate, ice_connection_state::RTCIceConnectionState,
        ice_server::RTCIceServer,
    },
    interceptor::registry::Registry,
    media::io::ogg_writer::OggWriter,
    peer_connection::{configuration::RTCConfiguration, RTCPeerConnection},
    rtp_transceiver::{
        rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
        rtp_transceiver_direction::RTCRtpTransceiverDirection,
        RTCRtpTransceiverInit,
    },
    track::track_remote::TrackRemote,
};

lazy_static::lazy_static! {
    static ref PENDING_CANDIDATES: Arc<Mutex<Vec<RTCIceCandidate>>> = Arc::new(Mutex::new(vec![]));
}

async fn save_to_disk(
    writer: Arc<Mutex<dyn webrtc::media::io::Writer + Send + Sync>>,
    track: Arc<TrackRemote>,
    notify: Arc<Notify>,
) -> color_eyre::Result<()> {
    loop {
        tokio::select! {
            result = track.read_rtp() => {
                if let Ok((rtp_packet, _)) = result {
                    let mut w = writer.lock().await;
                    w.write_rtp(&rtp_packet)?;
                }else{
                    info!("file closing begin after read_rtp error");
                    let mut w = writer.lock().await;
                    if let Err(err) = w.close() {
                        error!("file close err: {err}");
                    }
                    info!("file closing end after read_rtp error");
                    return Ok(());
                }
            }
            _ = notify.notified() => {
                info!("file closing begin after notified");
                let mut w = writer.lock().await;
                if let Err(err) = w.close() {
                    error!("file close err: {err}");
                }
                info!("file closing end after notified");
                return Ok(());
            }
        }
    }
}

// FIXME: This is for now just a reference
pub async fn start_webrtc_call_to_sip(room: Room) -> color_eyre::Result<Arc<RTCPeerConnection>> {
    let ogg_writer: Arc<Mutex<dyn webrtc::media::io::Writer + Send + Sync>> = Arc::new(Mutex::new(
        OggWriter::new(File::create("./test.opus")?, 48000, 2)?,
    ));
    // Create a MediaEngine object to configure the supported codec
    let mut m = MediaEngine::default();

    // We only add SIP save codecs here
    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;

    // m.register_codec(
    //     RTCRtpCodecParameters {
    //         capability: RTCRtpCodecCapability {
    //             mime_type: MIME_TYPE_PCMU.to_owned(),
    //             clock_rate: 8000,
    //             channels: 0,
    //             sdp_fmtp_line: "".to_owned(),
    //             rtcp_feedback: vec![],
    //         },
    //         payload_type: 0,
    //         ..Default::default()
    //     },
    //     RTPCodecType::Audio,
    // )?;

    // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
    // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
    // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
    // for each PeerConnection.
    let mut registry = Registry::new();

    // Use the default set of Interceptors
    registry = register_default_interceptors(registry, &mut m)?;

    // Create the API object with the MediaEngine
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    // Prepare the configuration
    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    // Create a new RTCPeerConnection
    let peer_connection = Arc::new(api.new_peer_connection(config).await?);

    // This must exist.
    peer_connection
        .add_transceiver_from_kind(
            RTPCodecType::Audio,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await?;

    // Prepare udp conns
    // Also update incoming packets with expected PayloadType, the browser may use
    // a different value. We have to modify so our stream matches what rtp-forwarder.sdp expects
    // FIXME: This should handle the incoming stream. Which I think should be matrix
    // let mut udp_conns = HashMap::new();
    // udp_conns.insert(
    //     "audio".to_owned(),
    //     UdpConn {
    //         conn: {
    //             let sock = UdpSocket::bind("127.0.0.1:0").await?;
    //             sock.connect(format!("127.0.0.1:{}", 4000)).await?;
    //             Arc::new(sock)
    //         },
    //         payload_type: 111,
    //     },
    // );

    // Set a handler for when a new remote track starts, this handler will forward data to
    // our UDP listeners.
    // In your application this is where you would handle/process audio/video
    //let pc = Arc::downgrade(&peer_connection);
    let notify_tx = Arc::new(Notify::new());
    let notify_rx = notify_tx.clone();
    peer_connection.on_track(Box::new(move |track, _, _| {
        // Retrieve udp connection
        // TODO: Use this to get the matrix connection?
        // let c = if let Some(c) = udp_conns.get(&track.kind().to_string()) {
        //     c.clone()
        // } else {
        //     return Box::pin(async {});
        // };

        // Send a PLI on an interval so that the publisher is pushing a keyframe every rtcpPLIInterval
        //let media_ssrc = track.ssrc();
        //let pc2 = pc.clone();
        // tokio::spawn(async move {
        //     let mut result = color_eyre::Result::<usize>::Ok(0);
        //     while result.is_ok() {
        //         let timeout = tokio::time::sleep(Duration::from_secs(3));
        //         tokio::pin!(timeout);

        //         tokio::select! {
        //             _ = timeout.as_mut() =>{
        //                 if let Some(pc) = pc2.upgrade(){
        //                     result = pc.write_rtcp(&[Box::new(PictureLossIndication{
        //                         sender_ssrc: 0,
        //                         media_ssrc,
        //                     })]).await.map_err(Into::into);
        //                 }else{
        //                     break;
        //                 }
        //             }
        //         };
        //     }
        // });

        // tokio::spawn(async move {
        //     let mut b = vec![0u8; 1500];
        //     while let Ok((mut rtp_packet, _)) = track.read(&mut b).await {
        //         // Update the PayloadType
        //         rtp_packet.header.payload_type = c.payload_type;

        //         // Marshal into original buffer with updated PayloadType

        //         let n = rtp_packet.marshal_to(&mut b)?;

        //         // Write
        //         if let Err(err) = c.conn.send(&b[..n]).await {
        //             // For this particular example, third party applications usually timeout after a short
        //             // amount of time during which the user doesn't have enough time to provide the answer
        //             // to the browser.
        //             // That's why, for this particular example, the user first needs to provide the answer
        //             // to the browser then open the third party application. Therefore we must not kill
        //             // the forward on "connection refused" errors
        //             //if opError, ok := err.(*net.OpError); ok && opError.Err.Error() == "write: connection refused" {
        //             //    continue
        //             //}
        //             //panic(err)
        //             if err.to_string().contains("Connection refused") {
        //                 continue;
        //             } else {
        //                 error!("conn send err: {err}");
        //                 break;
        //             }
        //         }
        //     }

        //     color_eyre::Result::<()>::Ok(())
        // });

        //TODO: Remove when going live
        let notify_rx2 = Arc::clone(&notify_rx);
        let ogg_writer2 = Arc::clone(&ogg_writer);
        Box::pin(async move {
            let codec = track.codec();
            let mime_type = codec.capability.mime_type.to_lowercase();
            if mime_type == MIME_TYPE_OPUS.to_lowercase() {
                info!("Got Opus track, saving to disk as output.opus (48 kHz, 2 channels)");
                tokio::spawn(async move {
                    let _ = save_to_disk(ogg_writer2, track, notify_rx2).await;
                });
            }
        })

        //Box::pin(async {})
    }));

    // Set the handler for ICE connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection.on_ice_connection_state_change(Box::new(
        move |connection_state: RTCIceConnectionState| {
            info!("Connection State has changed {connection_state}");
            if connection_state == RTCIceConnectionState::Failed {
                notify_tx.notify_waiters();
                info!("PeerConnection failed, closing");
                let error_message =
                    RoomMessageEventContent::notice_plain(format!("🚨Call ended🚨",));
                let room_clone = room.clone();
                return Box::pin(async move {
                    room_clone
                        .clone()
                        .send(error_message, None)
                        .await
                        .expect("Failed to send error message");
                });
            }
            Box::pin(async {})
        },
    ));

    Ok(peer_connection)
}
