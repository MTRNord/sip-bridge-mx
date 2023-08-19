use std::time::Duration;

use clap::Parser;
use matrix_sdk::{
    config::SyncSettings,
    event_handler::Ctx,
    matrix_auth::{Session, SessionTokens},
    ruma::{
        events::room::{
            member::StrippedRoomMemberEvent,
            message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
        },
        OwnedUserId,
    },
    Client, Room, RoomState, SessionMeta,
};
use secrecy::{ExposeSecret, SecretString};
use sip::SIPRequest;
use tokio::time::sleep;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{sip::send_invite, webrtc::start_webrtc_call_to_sip};

mod db;
mod sip;
mod webrtc;

#[derive(Parser)]
#[command(author,version, about, long_about= None)]
struct Cli {
    homeserver_url: String,
    username: String,
    password: SecretString,
    sip_server_domain: String,
    sip_server_address: String,
    sip_username: String,
    sip_password: SecretString,
}

#[derive(Debug, Clone)]
pub struct SipAuthInfo {
    pub address: String,
    pub username: String,
    pub password: SecretString,
}

#[derive(Debug, Clone)]
pub struct CallID(pub String);

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let (matrix_side_ws_sender, ws_side_matrix_receiver) =
        tokio::sync::mpsc::channel::<SIPRequest>(4096);

    // TODO: This probably has to be per call instead
    let call_id = CallID(Uuid::new_v4().to_string());
    let call_id_clone = call_id.clone();
    let auth_info = SipAuthInfo {
        address: cli.sip_server_address.clone(),
        username: cli.sip_username.clone(),
        password: cli.sip_password.clone(),
    };
    tokio::spawn(async move {
        // TODO: Handle errors
        sip::handle_sip_connection(
            ws_side_matrix_receiver,
            call_id_clone,
            cli.sip_server_domain,
            cli.sip_server_address,
            cli.sip_username,
            cli.sip_password,
        )
        .await
        .unwrap();
    });

    login_and_sync(
        cli.homeserver_url,
        &cli.username,
        cli.password,
        call_id.clone(),
        matrix_side_ws_sender,
        auth_info,
    )
    .await?;

    Ok(())
}

async fn login_and_sync(
    homeserver_url: String,
    username: &str,
    password: SecretString,
    call_id: CallID,
    ws_sender: tokio::sync::mpsc::Sender<SIPRequest>,
    auth_info: SipAuthInfo,
) -> color_eyre::Result<()> {
    // Note that when encryption is enabled, you should use a persistent store to be
    // able to restore the session with a working encryption setup.
    // See the `persist_session` example.
    let client = Client::builder()
        .homeserver_url(homeserver_url)
        .sqlite_store("./store", None)
        .build()
        .await?;

    let db = db::setup_db().await?;
    if let Some((token, device_id)) = db.get_user(username.to_string()).await? {
        client
            .matrix_auth()
            .restore_session(Session {
                meta: SessionMeta {
                    user_id: OwnedUserId::try_from(username).unwrap(),
                    device_id: device_id.into(),
                },
                tokens: SessionTokens {
                    access_token: token,
                    refresh_token: None,
                },
            })
            .await?;
    } else {
        client
            .matrix_auth()
            .login_username(username, password.expose_secret())
            .initial_device_display_name("autojoin bot")
            .await?;
        db.set_user(
            client.user_id().unwrap().to_string(),
            client.access_token().unwrap().to_string(),
            client.device_id().unwrap().to_string(),
        )
        .await?;
    }

    println!("logged in as {username}");

    client.add_event_handler_context(ws_sender.clone());
    client.add_event_handler_context(call_id.clone());
    client.add_event_handler_context(auth_info.clone());
    client.add_event_handler(on_stripped_state_member);
    client.add_event_handler(on_room_message);

    info!("Syncing...");
    client.sync(SyncSettings::default()).await?;

    warn!("Syncing crashed");
    Ok(())
}

// This handles autojoining rooms that the bot is invited to.
async fn on_stripped_state_member(
    room_member: StrippedRoomMemberEvent,
    client: Client,
    room: Room,
) {
    if room_member.state_key != client.user_id().unwrap() {
        return;
    }

    tokio::spawn(async move {
        info!("Autojoining room {}", room.room_id());
        let mut delay = 2;

        while let Err(err) = room.join().await {
            // retry autojoin due to synapse sending invites, before the
            // invited user can join for more information see
            // https://github.com/matrix-org/synapse/issues/4345
            error!(
                "Failed to join room {} ({err:?}), retrying in {delay}s",
                room.room_id()
            );

            sleep(Duration::from_secs(delay)).await;
            delay *= 2;

            if delay > 3600 {
                error!("Can't join room {} ({err:?})", room.room_id());
                break;
            }
        }
        info!("Successfully joined room {}", room.room_id());
    });
}

// Handle commands for dial-out, dial-in and help
async fn on_room_message(
    event: OriginalSyncRoomMessageEvent,
    room: Room,
    ws_sender: Ctx<tokio::sync::mpsc::Sender<SIPRequest>>,
    call_id: Ctx<CallID>,
    auth_info: Ctx<SipAuthInfo>,
) {
    if room.state() != RoomState::Joined {
        return;
    }
    let MessageType::Text(text_content) = event.content.msgtype else {
        return;
    };

    info!("Received message: {}", text_content.body);

    // Handle the Help command
    if text_content.body.starts_with("!help") {
        let help_message = RoomMessageEventContent::text_html("Commands:\n\
                            !dial-out <sip_address> - Dial a SIP address\n\
                            !dial-in - Allow dial-in via a code and password over SIP into the current conference (starts a new one if needed)\n\
                            !help - Show this help message",  
                            //Version using html lists
                            "<ul>\
                            <li><code>!dial-out &lt;sip_address&gt;</code> - Dial a SIP address</li>\
                            <li><code>!dial-in</code> - Allow dial-in via a code and password over SIP into the current conference (starts a new one if needed)</li>\
                            <li><code>!help</code> - Show this help message</li>\
                            </ul>"
                            );

        room.send(help_message, None).await.unwrap();
    } else if text_content.body.starts_with("!dial-out") {
        // Handle the dial-out command
        let sip_address = text_content.body.split(' ').nth(1).unwrap();

        // Check if sip address starts with `sip:`, or `sips:` or is a number (without `+`)
        // If it starts with `sip:` or `sips:` we check if it has a valid domain name after the `@` or an IP address
        // Since we don't need major validation, we just check if it has a `@` and a `.`.
        // For numbers there are no additional checks
        if !((sip_address.starts_with("sip:") || sip_address.starts_with("sips:"))
            && sip_address.contains('@')
            && sip_address.contains('.'))
            && !sip_address.chars().all(char::is_numeric)
        {
            let call_message = RoomMessageEventContent::notice_plain(format!(
                "Invalid SIP address: {}. Please use a SIP address starting with `sip:` or `sips:` or a number (without `+`)",
                sip_address
            ));

            room.send(call_message, None).await.unwrap();
            return;
        }

        let call_message =
            RoomMessageEventContent::notice_plain(format!("Starting to dial {}", sip_address));

        room.send(call_message, None).await.unwrap();

        let peer = start_webrtc_call_to_sip(room.clone()).await.unwrap();
        if let Err(e) = send_invite(
            sip_address.to_owned(),
            ws_sender.0,
            call_id.0.clone(),
            peer.clone(),
            room.clone(),
            auth_info.0,
        )
        .await
        {
            error!("Error sending invite: {}", e);
            let call_message: RoomMessageEventContent =
                RoomMessageEventContent::notice_plain(format!(
                    "🚨Failed to establish call to {}. Aborting attempts🚨",
                    sip_address
                ));

            room.send(call_message, None).await.unwrap();
            let _ = peer.close().await;
            return;
        }
    } else if text_content.body.starts_with("!dial-in") {
        // Handle the dial-in command
        //let call_id = dial_in().await;

        let call_message = RoomMessageEventContent::notice_plain(format!(
            "Dialing in enabled via sip address: {}\n\
            Code: {}\n\
            Password: {}",
            // TODO: Properly generate these
            "sip:example.com",
            1234,
            "password"
        ));

        room.send(call_message, None).await.unwrap();
    }
}
