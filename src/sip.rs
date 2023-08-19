use std::{future::Future, sync::Arc, time::Duration};

use fastwebsockets::{handshake, FragmentCollector, OpCode, Payload};
use hyper::{
    header::{CONNECTION, UPGRADE},
    upgrade::Upgraded,
    Body, Request, Version,
};
use local_ip_address::local_ip;
use matrix_sdk::{ruma::events::room::message::RoomMessageEventContent, Room};
use rsip::{
    prelude::{HasHeaders, HeadersExt, UntypedHeader},
    services::DigestGenerator,
    Header,
};
use secrecy::{ExposeSecret, SecretString};
use tokio::{net::TcpStream, time::sleep};
use tokio_rustls::{
    rustls::{ClientConfig, OwnedTrustAnchor},
    TlsConnector,
};
use tracing::{error, info, warn};
use uuid::Uuid;
use webrtc::peer_connection::{
    peer_connection_state::RTCPeerConnectionState, sdp::session_description::RTCSessionDescription,
    RTCPeerConnection,
};

use crate::{CallID, SipAuthInfo};

fn tls_connector() -> color_eyre::Result<TlsConnector> {
    let mut root_store = tokio_rustls::rustls::RootCertStore::empty();

    root_store.add_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| {
        OwnedTrustAnchor::from_subject_spki_name_constraints(
            ta.subject,
            ta.spki,
            ta.name_constraints,
        )
    }));

    let config = ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Ok(TlsConnector::from(Arc::new(config)))
}

// Connect to the SIP server using a websocket connection and rsip library
async fn connect(sip_server_domain: String) -> color_eyre::Result<FragmentCollector<Upgraded>> {
    let tcp_stream = TcpStream::connect(format!("{sip_server_domain}:443")).await?;
    let tls_connector = tls_connector()?;
    let domain = tokio_rustls::rustls::ServerName::try_from(sip_server_domain.as_str())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid dnsname"))?;

    let tls_stream = tls_connector.connect(domain, tcp_stream).await?;

    let req = Request::builder()
        .method("GET")
        .uri("/ws")
        .version(Version::HTTP_11)
        .header("Host", sip_server_domain)
        .header(UPGRADE, "websocket")
        .header(CONNECTION, "Upgrade")
        .header("Sec-WebSocket-Key", handshake::generate_key())
        .header("Sec-WebSocket-Protocol", "sip")
        .header("Sec-WebSocket-Version", "13")
        .body(Body::empty())?;

    let (ws, _) = handshake::client(&SpawnExecutor, req, tls_stream).await?;
    Ok(FragmentCollector::new(ws))
}

// Tie hyper's executor to tokio runtime
struct SpawnExecutor;

impl<Fut> hyper::rt::Executor<Fut> for SpawnExecutor
where
    Fut: Future + Send + 'static,
    Fut::Output: Send + 'static,
{
    fn execute(&self, fut: Fut) {
        tokio::task::spawn(fut);
    }
}

pub async fn handle_sip_connection(
    mut outgoing_messages: tokio::sync::mpsc::Receiver<SIPRequest>,
    call_id: CallID,
    sip_server_domain: String,
    sip_server: String,
    sip_username: String,
    sip_password: SecretString,
) -> color_eyre::Result<()> {
    let mut ws = connect(sip_server_domain).await?;
    info!("Connected to SIP server");
    info!("Sending register request");

    // Register with SIP server
    let base_register_request =
        generate_base_register_request(call_id, sip_server.clone(), sip_username.clone())?;
    ws.write_frame(fastwebsockets::Frame::text(
        fastwebsockets::Payload::Borrowed(base_register_request.to_string().as_bytes()),
    ))
    .await?;

    // Get the response
    let frame = ws.read_frame().await?;
    let response = if let OpCode::Text = frame.opcode {
        let response = String::from_utf8(frame.payload.to_vec())?;
        info!("Got response: {}", response);
        rsip::Response::try_from(response.as_str())?
    } else {
        error!(
            "Got unexpected response: {:?} - {:?}",
            frame.opcode, frame.payload
        );
        return Err(color_eyre::eyre::eyre!("Got unexpected response"));
    };

    let register_request = generate_request_with_auth(
        response,
        None,
        base_register_request,
        sip_server,
        sip_username,
        sip_password,
    )?;
    ws.write_frame(fastwebsockets::Frame::text(
        fastwebsockets::Payload::Borrowed(register_request.to_string().as_bytes()),
    ))
    .await?;

    // Get the response
    let frame = ws.read_frame().await?;
    if let OpCode::Text = frame.opcode {
        let response = String::from_utf8(frame.payload.to_vec())?;
        info!("Got response: {}", response);
    } else {
        error!(
            "Got unexpected response: {:?} - {:?}",
            frame.opcode, frame.payload
        );
        return Err(color_eyre::eyre::eyre!("Got unexpected response"));
    }

    loop {
        tokio::select! {
            Ok(frame) = ws.read_frame() => {
                if let OpCode::Close = frame.opcode {
                    break;
                }

                // Send out message if we have one
                if let Ok(message) = outgoing_messages.try_recv() {
                    ws.write_frame(fastwebsockets::Frame::text(
                        fastwebsockets::Payload::Borrowed(message.message.to_string().as_bytes()),
                    ))
                    .await?;

                    let mut responses = Vec::with_capacity(message.response_count + 1);
                    for _ in 0..message.response_count {
                        let frame =  ws.read_frame().await?;
                        match frame.opcode {
                            OpCode::Text | OpCode::Binary => {
                                if !frame.payload.is_empty() {
                                    if let Ok(message) = String::from_utf8(frame.payload.to_vec()) {
                                        // Empty and BYE responses cannot be parsed
                                        if !message.is_empty() && !message.starts_with("BYE") && message != "\r\n\r\n" {
                                            //debug!("Got message: {:?}", message);
                                            responses.push(rsip::Response::try_from(message.as_str())?);
                                        }
                                    } else {
                                        warn!("Got invalid UTF-8 message");
                                    }
                                }
                            }
                            _ => {}
                        }
                    }

                   let _ = message.response_channel.send(responses);
                }
           },
           _ = sleep(Duration::from_millis(500)) => {
                // Keep the connection alive
                ws.write_frame(fastwebsockets::Frame::new(
                    true,
                    OpCode::Ping,
                    None,
                    Payload::Owned(vec![]),
                ))
                .await?;
           },
        }
    }

    Ok(())
}

fn generate_base_register_request(
    call_id: CallID,
    sip_server: String,
    sip_username: String,
) -> color_eyre::Result<rsip::SipMessage> {
    let mut headers: rsip::Headers = Default::default();

    let base_uri = rsip::Uri {
        // TODO: Is it sip or Sips?
        scheme: Some(rsip::Scheme::Sip),
        auth: Some((sip_username.clone(), None::<String>).into()),
        host_with_port: rsip::Domain::from(sip_server.clone()).into(),
        ..Default::default()
    };

    let my_local_ip = local_ip()?;
    headers.push(
        rsip::typed::Via {
            version: rsip::Version::V2,
            transport: rsip::Transport::Wss,
            uri: rsip::Uri {
                host_with_port: (rsip::Domain::from(my_local_ip.to_string()), 5060).into(),
                ..Default::default()
            },
            params: vec![rsip::Param::Branch(rsip::param::Branch::new(
                // TODO: This should be generated. Note that `z9hG4bK` is a cookie and the whole thing MUST be unique for every request we send
                "z9hG4bKnashds7",
            ))],
        }
        .into(),
    );

    headers.push(rsip::headers::MaxForwards::default().into());

    headers.push(
        rsip::typed::From {
            display_name: Some(sip_username.clone()),
            uri: base_uri.clone(),
            params: vec![rsip::Param::Tag(rsip::param::Tag::new("a73kszlfl"))],
        }
        .into(),
    );

    headers.push(
        rsip::typed::To {
            display_name: Some(sip_username.clone()),
            uri: base_uri.clone(),
            params: Default::default(),
        }
        .into(),
    );
    headers.push(rsip::headers::CallId::new(call_id.0).into());

    headers.push(
        rsip::typed::CSeq {
            // TODO: I guess we need to keep track of this?
            seq: 1,
            method: rsip::Method::Register,
        }
        .into(),
    );
    headers.push(
        rsip::typed::Contact {
            display_name: None,
            uri: base_uri,
            params: Default::default(),
        }
        .into(),
    );

    headers.push(rsip::headers::UserAgent::new("matrix_sip_bridge").into());
    headers.push(rsip::headers::ContentLength::default().into());

    Ok(rsip::Request {
        method: rsip::Method::Register,
        uri: rsip::Uri {
            scheme: Some(rsip::Scheme::Sip),
            host_with_port: rsip::Domain::from(sip_server).into(),
            ..Default::default()
        },
        version: rsip::Version::V2,
        headers,
        body: Default::default(),
    }
    .into())
}

pub fn generate_request_with_auth(
    response: rsip::Response,
    auth: Option<rsip::Auth>,
    mut base_request: rsip::SipMessage,
    sip_server: String,
    sip_username: String,
    sip_password: SecretString,
) -> color_eyre::Result<rsip::SipMessage> {
    let www_authenticate_header: rsip::headers::typed::WwwAuthenticate = response
        .headers()
        .iter()
        .find_map(|header| match header {
            Header::WwwAuthenticate(header) => Some(header.clone()),
            _ => None,
        })
        .ok_or_else(|| color_eyre::eyre::eyre!("Missing WWW-Authenticate header"))?
        .try_into()?;

    let cnonce = Uuid::new_v4().to_string();

    // We can safely assume its a request message and extract the method
    let method = match base_request {
        rsip::SipMessage::Request(ref request) => request.method,
        _ => {
            return Err(color_eyre::eyre::eyre!(
                "Expected a request message but got a response"
            ))
        }
    };

    let auth_digest = DigestGenerator {
        username: &sip_username,
        password: sip_password.expose_secret(),
        realm: &www_authenticate_header.realm,
        nonce: &www_authenticate_header.nonce,
        uri: &rsip::Uri {
            scheme: Some(rsip::Scheme::Sip),
            host_with_port: rsip::Domain::from(sip_server.clone()).into(),
            auth: auth.clone(),
            ..Default::default()
        },
        method: &method,
        qop: Some(rsip::headers::auth::AuthQop::Auth {
            cnonce: cnonce.clone(),
            nc: 1,
        })
        .as_ref(),
        algorithm: www_authenticate_header
            .algorithm
            .ok_or_else(|| color_eyre::eyre::eyre!("WWW-Authenticate header missing algorithm"))?,
    }
    .compute();

    let auth_header = rsip::headers::typed::Authorization {
        scheme: rsip::headers::auth::Scheme::Digest,
        username: sip_username,
        realm: www_authenticate_header.realm,
        nonce: www_authenticate_header.nonce,
        uri: rsip::Uri {
            scheme: Some(rsip::Scheme::Sip),
            host_with_port: rsip::Domain::from(sip_server).into(),
            auth,
            ..Default::default()
        },
        response: auth_digest,
        opaque: www_authenticate_header.opaque,
        algorithm: www_authenticate_header.algorithm,
        qop: Some(rsip::headers::auth::AuthQop::Auth { cnonce, nc: 1 }),
    };

    if let rsip::SipMessage::Request(request) = &mut base_request {
        request.headers_mut().push(auth_header.into());
    }
    // We also need to increase the CSeq
    let cseq = base_request.cseq_header_mut()?;
    cseq.mut_seq(cseq.seq()? + 1)?;

    Ok(base_request)
}

pub fn generate_new_branch(base_request: rsip::Request) -> color_eyre::Result<rsip::SipMessage> {
    // Change the branch param in the via header
    let via_header = base_request.via_header()?;
    let new_params: Vec<_> = via_header
        .params()?
        .iter()
        .map(|param| match param {
            rsip::Param::Branch(_) => rsip::Param::Branch(rsip::param::Branch::new("12345")),
            _ => param.clone(),
        })
        .collect();
    // We cant mutate the params directly so we have to replace the whole thing
    let new_header = rsip::typed::Via {
        version: via_header.version()?,
        transport: via_header.trasnport()?,
        uri: via_header.uri().clone()?,
        params: new_params,
    };
    // Sadly we can also not replace the via so we need to recreate the whole request

    let headers: Vec<_> = base_request
        .headers()
        .iter()
        .map(|header| match header {
            Header::Via(_) => Header::Via(new_header.clone().into()),
            _ => header.clone(),
        })
        .collect();
    Ok(rsip::Request {
        method: base_request.method,
        uri: base_request.uri.clone(),
        version: base_request.version,
        headers: headers.into(),
        body: base_request.body.clone(),
    }
    .into())
}

/// We first generate a request and expect a response with a WWW-Authenticate header
/// We then return an ACK and then a new INVITE request with the Authorization header
pub async fn generate_invite_base_request(
    call_id: CallID,
    sip_uri: rsip::Uri,
    sdp: String,
    sip_server: String,
    sip_username: String,
) -> color_eyre::Result<rsip::SipMessage> {
    let mut headers: rsip::Headers = Default::default();

    let base_uri = rsip::Uri {
        // TODO: Is it sip or Sips?
        scheme: Some(rsip::Scheme::Sip),
        auth: Some((sip_username, None::<String>).into()),
        host_with_port: rsip::Domain::from(sip_server).into(),
        ..Default::default()
    };

    let my_local_ip = local_ip()?;
    headers.push(
        rsip::typed::Via {
            version: rsip::Version::V2,
            transport: rsip::Transport::Wss,
            uri: rsip::Uri {
                host_with_port: my_local_ip.into(),
                ..Default::default()
            },
            params: vec![rsip::Param::Branch(rsip::param::Branch::new(
                // TODO: This should be generated. Note that `z9hG4bK` is a cookie and the whole thing MUST be unique for every request we send
                "z9hG4bKnashds7",
            ))],
        }
        .into(),
    );

    headers.push(
        rsip::typed::From {
            display_name: Some("sip_bridge".into()),
            uri: base_uri.clone(),
            params: vec![rsip::Param::Tag(rsip::param::Tag::new("a73kszlfl"))],
        }
        .into(),
    );

    headers.push(
        rsip::typed::To {
            display_name: None,
            uri: sip_uri.clone(),
            params: Default::default(),
        }
        .into(),
    );

    headers.push(rsip::headers::CallId::new(call_id.0).into());

    headers.push(
        rsip::typed::CSeq {
            // TODO: I guess we need to keep track of this?
            seq: 1,
            method: rsip::Method::Invite,
        }
        .into(),
    );

    headers.push(
        rsip::typed::Contact {
            display_name: None,
            uri: base_uri,
            params: vec![rsip::Param::Transport(rsip::Transport::Wss)],
        }
        .into(),
    );

    headers.push(rsip::headers::MaxForwards::default().into());
    headers.push(rsip::headers::ContentType::new("application/sdp").into());

    headers.push(rsip::headers::UserAgent::new("matrix_sip_bridge").into());

    headers.push(rsip::headers::ContentLength::new(sdp.len().to_string()).into());

    Ok(rsip::Request {
        method: rsip::Method::Invite,
        uri: sip_uri,
        version: rsip::Version::V2,
        headers,
        body: sdp.as_bytes().to_vec(),
    }
    .into())
}

pub fn generate_ack_request(
    call_id: CallID,
    cseq: u32,
    response_uri: rsip::Uri,
    branch: rsip::param::Branch,
    sip_uri: rsip::Uri,
    sip_server: String,
    sip_username: String,
) -> color_eyre::Result<rsip::SipMessage> {
    let mut headers: rsip::Headers = Default::default();

    let base_uri = rsip::Uri {
        // TODO: Is it sip or Sips?
        scheme: Some(rsip::Scheme::Sip),
        auth: Some((sip_username, None::<String>).into()),
        host_with_port: rsip::Domain::from(sip_server).into(),
        ..Default::default()
    };

    let my_local_ip = local_ip()?;
    headers.push(
        rsip::typed::Via {
            version: rsip::Version::V2,
            transport: rsip::Transport::Wss,
            uri: rsip::Uri {
                host_with_port: my_local_ip.into(),
                ..Default::default()
            },
            params: vec![rsip::Param::Branch(branch)],
        }
        .into(),
    );

    headers.push(
        rsip::typed::From {
            display_name: Some("sip_bridge".into()),
            uri: base_uri.clone(),
            params: vec![rsip::Param::Tag(rsip::param::Tag::new("a73kszlfl"))],
        }
        .into(),
    );

    headers.push(
        rsip::typed::To {
            display_name: None,
            uri: sip_uri.clone(),
            params: Default::default(),
        }
        .into(),
    );

    headers.push(rsip::headers::CallId::new(call_id.0).into());

    headers.push(
        rsip::typed::CSeq {
            // TODO: I guess we need to keep track of this?
            seq: cseq,
            method: rsip::Method::Ack,
        }
        .into(),
    );

    headers.push(rsip::headers::MaxForwards::default().into());
    headers.push(rsip::headers::ContentLength::default().into());

    Ok(rsip::Request {
        method: rsip::Method::Ack,
        uri: response_uri,
        version: rsip::Version::V2,
        headers,
        body: Default::default(),
    }
    .into())
}

pub struct SIPRequest {
    message: rsip::SipMessage,
    /// This is the count of responses that are going to be returned. This is NOT the count we need to use
    response_count: usize,
    response_channel: tokio::sync::oneshot::Sender<Vec<rsip::Response>>,
}

impl SIPRequest {
    fn new(
        message: rsip::SipMessage,
        response_count: usize,
    ) -> (Self, tokio::sync::oneshot::Receiver<Vec<rsip::Response>>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (
            SIPRequest {
                message,
                response_count,
                response_channel: tx,
            },
            rx,
        )
    }
}

pub async fn send_invite(
    sip_address: String,
    ws_sender: tokio::sync::mpsc::Sender<SIPRequest>,
    call_id: CallID,
    peer_connection: Arc<RTCPeerConnection>,
    room: Room,
    auth_info: SipAuthInfo,
) -> color_eyre::Result<()> {
    info!("Gathering ICE candidates");

    // Set the handler for Peer connection state
    // This will notify you when the peer has connected/disconnected
    let address_clone = sip_address.clone();
    let room_clone = room.clone();
    peer_connection.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        info!("Peer Connection State has changed: {s}");

        let room_clone = room_clone.clone();
        if s == RTCPeerConnectionState::Failed {
            // Wait until PeerConnection has had no network activity for 30 seconds or another failure. It may be reconnected using an ICE Restart.
            // Use webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster timeout.
            // Note that the PeerConnection may come back from PeerConnectionStateDisconnected.
            info!("Peer Connection has gone to failed exiting: Done forwarding");

            let error_message = RoomMessageEventContent::notice_plain(format!(
                "🚨Call to {} has ended🚨",
                address_clone.clone()
            ));
            return Box::pin(async move {
                room_clone
                    .clone()
                    .send(error_message, None)
                    .await
                    .expect("Failed to send error message");
            });
        } else if s == RTCPeerConnectionState::Connected {
            let call_message = RoomMessageEventContent::notice_plain(format!(
                "Call to {} connected",
                address_clone.clone()
            ));
            return Box::pin(async move {
                room_clone
                    .clone()
                    .send(call_message, None)
                    .await
                    .expect("Failed to send error message");
            });
        } else if s == RTCPeerConnectionState::Connecting {
            let call_message = RoomMessageEventContent::notice_plain(format!(
                "Call to {} connecting",
                address_clone.clone()
            ));
            return Box::pin(async move {
                room_clone
                    .clone()
                    .send(call_message, None)
                    .await
                    .expect("Failed to send error message");
            });
        }

        Box::pin(async {})
    }));

    peer_connection.on_negotiation_needed(Box::new(move || {
        error!("Negotiation needed");
        Box::pin(async {})
    }));

    // Create an offer to send to the sip side
    let offer = peer_connection.create_offer(None).await?;
    peer_connection.set_local_description(offer.clone()).await?;

    // Create channel that is blocked until ICE Gathering is complete
    let mut gather_complete = peer_connection.gathering_complete_promise().await;

    // Block until ICE Gathering is complete, disabling trickle ICE
    // we do this because we only can exchange one signaling message
    // in a production application you should exchange ICE Candidates via OnICECandidate
    let _ = gather_complete.recv().await;

    info!("Got ICE candidates");

    let offer = peer_connection.create_offer(None).await?;
    peer_connection.set_local_description(offer.clone()).await?;

    info!("Sending offer: {}", offer.sdp);

    // Get the user, scheme and domain from the sip address if it is starting with sip: or sips:
    // Otherwise treat it as a number for the user. The domain will be the default domain
    let uri = if sip_address.starts_with("sip:") {
        rsip::Uri::try_from(sip_address.clone())?
    } else {
        rsip::Uri {
            scheme: Some(rsip::Scheme::Sip),
            host_with_port: rsip::Domain::from(auth_info.address.clone()).into(),
            auth: Some(rsip::Auth {
                user: sip_address.to_owned(),
                password: None,
            }),
            ..Default::default()
        }
    };

    // Send invite to SIP Side
    let invite_request = generate_invite_base_request(
        call_id.clone(),
        uri.clone(),
        offer.sdp,
        auth_info.address.clone(),
        auth_info.username.clone(),
    )
    .await?;

    // Send base request
    let (ws_request, responses) = SIPRequest::new(invite_request.clone(), 1);
    ws_sender.send(ws_request).await?;

    let responses = responses.await?;
    let invite_response = match responses.first() {
        Some(response) => response,
        None => {
            return Err(color_eyre::eyre::eyre!("Got no response on invite"));
        }
    };
    info!("Got invite response: {}", invite_response);

    // Send ack
    // Get response branch of via header
    let params = invite_response.via_header()?.params()?;
    let via_branch = match params.iter().find_map(|param| match param {
        rsip::param::Param::Branch(branch) => Some(branch),
        _ => None,
    }) {
        Some(branch) => branch,
        None => {
            return Err(color_eyre::eyre::eyre!("Got no branch in response"));
        }
    };
    let ack = generate_ack_request(
        call_id.clone(),
        1,
        uri.clone(),
        via_branch.clone(),
        uri.clone(),
        auth_info.address.clone(),
        auth_info.username.clone(),
    )?;
    let (ws_request, _) = SIPRequest::new(ack.clone(), 0);
    ws_sender.send(ws_request).await?;

    // Send new invite with auth
    // Extract request from sipmessage (we know its a request because we just sent one)
    let invite_request = match invite_request {
        rsip::SipMessage::Request(request) => request,
        _ => {
            return Err(color_eyre::eyre::eyre!(
                "Expected a request message but got a response"
            ))
        }
    };
    let invite_request = generate_new_branch(invite_request)?;
    let invite_with_auth_request = generate_request_with_auth(
        invite_response.clone(),
        uri.auth.clone(),
        invite_request,
        auth_info.address.clone(),
        auth_info.username.clone(),
        auth_info.password,
    )?;
    let (ws_request, invite_authed_responses) =
        SIPRequest::new(invite_with_auth_request.clone(), 2);
    ws_sender.send(ws_request).await?;

    let invite_authed_responses = invite_authed_responses.await?;
    let invite_authed_response = match invite_authed_responses.last() {
        Some(response) => response,
        None => {
            return Err(color_eyre::eyre::eyre!("Got no response on invite"));
        }
    };
    info!("Last invite authed response: {}", invite_authed_response);

    let resp_body = invite_authed_response.body();
    let resp_body = String::from_utf8(resp_body.to_vec())?;
    info!("Got response body: {}", resp_body);
    let answer = RTCSessionDescription::answer(resp_body)?;
    info!("Got answer: {:#?}", answer);

    peer_connection.set_remote_description(answer).await?;

    // Get the contact uri from the response
    let contact_uri = invite_authed_response.contact_header().unwrap().uri()?;
    // Send ack
    // Get response branch of via header
    let params = invite_authed_response.via_header()?.params()?;
    let via_branch = match params.iter().find_map(|param| match param {
        rsip::param::Param::Branch(branch) => Some(branch),
        _ => None,
    }) {
        Some(branch) => branch,
        None => {
            return Err(color_eyre::eyre::eyre!("Got no branch in response"));
        }
    };
    let ack = generate_ack_request(
        call_id,
        2,
        contact_uri,
        via_branch.clone(),
        uri,
        auth_info.address,
        auth_info.username,
    )?;

    let (ws_request, _) = SIPRequest::new(ack.clone(), 0);
    ws_sender.send(ws_request).await?;

    let call_message = RoomMessageEventContent::notice_plain(format!(
        "Dialed out to {} with call ID {}",
        sip_address, 0
    ));

    room.send(call_message, None).await?;

    Ok(())
}
