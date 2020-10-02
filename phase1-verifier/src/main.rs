use phase1_verifier::{
    logger::init_logger,
    verifier::{Verifier, VerifierRequest},
};

use futures_util::StreamExt;
use tokio::task;
use tracing::debug;
use warp::{ws::WebSocket, Filter, Rejection, Reply};

async fn ws_client_connection(ws: WebSocket, id: String) {
    let (_client_ws_sender, mut client_ws_rcv) = ws.split();

    // TODO (raychu86) update these hard-coded values
    let coordinator_api_url = "http://localhost:8000/api/coordinator";
    let view_key = "AViewKey1cWNDyYMjc9p78PnCderRx37b9pJr4myQqmmPeCfeiLf3";

    println!("dsjflsdjfldsjflsdjf");
    let verifier = Verifier::new(coordinator_api_url.to_string(), view_key.to_string()).unwrap();

    while let Some(result) = client_ws_rcv.next().await {
        match result {
            Ok(msg) => {
                println!("received message: {:?}", msg);

                if let Ok(message_string) = msg.to_str() {
                    // Check if the message can be deserialized into a verifier request
                    if let Ok(verifier_request) = serde_json::from_str::<VerifierRequest>(&message_string) {
                        if verifier_request.method.to_lowercase() == "lock" {
                            // Spawn a task to lock the chunk
                            let verifier_clone = verifier.clone();
                            task::spawn(async move {
                                if let Err(err) = verifier_clone.lock_chunk(verifier_request.chunk_id).await {
                                    debug!("Failed to lock chunk (error {})", err);
                                }
                            });
                        } else if verifier_request.method.to_lowercase() == "verify" {
                            // Spawn a task to verify a contribution in the chunk
                            let verifier_clone = verifier.clone();
                            task::spawn(async move {
                                if let Err(err) = verifier_clone.verify_contribution(verifier_request.chunk_id).await {
                                    debug!("Failed to verify chunk (error {})", err);
                                }
                            });
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("error receiving ws message for id: {}): {}", id.clone(), e);
                break;
            }
        };
    }
}

pub async fn ws_handler(ws: warp::ws::Ws, id: String) -> Result<impl Reply, Rejection> {
    Ok(ws.on_upgrade(move |socket| ws_client_connection(socket, id)))
}

#[tokio::main]
async fn main() {
    init_logger("TRACE");

    let ws_route = warp::path("ws")
        .and(warp::ws())
        .and(warp::path::param())
        .and_then(ws_handler);

    println!("Started on port 8080");
    warp::serve(ws_route).run(([0, 0, 0, 0], 8080)).await;
}
