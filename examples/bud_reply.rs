//! Wait for mail and answer it.
//!
//!     NOETIVE_KEY_SECRET=keya_... cargo run --example bud_reply
//!
//! Answers only new conversations from senders the service could verify,
//! never its own mail, and reports a refused or held send. Each reply carries an
//! idempotency key, so a retried send goes out once.

use noetive::bud::{ById, Client, Me, SendInput, WaitInput, EVENT_RECEIVED, JOIN_NEW, STATE_HELD};

#[tokio::main]
async fn main() -> Result<(), noetive::bud::Failure> {
    let client = Client::from_env()?; // reads NOETIVE_KEY_SECRET

    // Who am I? Every identifier starts here.
    let me: Me = client.describe_me().await?.into()?;
    println!("{} {:?}", me.agent, me.addresses);

    let mut cursor = String::new();
    loop {
        let batch = client
            .wait(WaitInput {
                cursor: cursor.clone(),
                ..WaitInput::default()
            })
            .await?; // Err: the connection failed; nothing to branch on
        if let Some(refusal) = batch.error {
            eprintln!("{refusal}"); // code says what happened, hint what to do next
            return Ok(());
        }
        cursor = batch.cursor; // an empty window is success: keep waiting from here

        for event in batch.events {
            if event.kind != EVENT_RECEIVED || me.addresses.contains(&event.data.from) {
                continue; // only new mail, and never our own
            }
            let message = client
                .describe_message(ById {
                    id: event.message.clone(),
                    ..ById::default()
                })
                .await?;
            let answerable = message
                .provenance
                .as_ref()
                .is_some_and(|p| p.aligned && p.thread_join == JOIN_NEW);
            if !answerable {
                continue; // unverified, or a reply: read it, do not answer it
            }
            let sent = client
                .send(SendInput {
                    in_reply_to: event.message.clone(),
                    text: "Got it, thanks.".into(),
                    idempotency_key: format!("reply-{}", event.id), // a retried send goes out once
                    ..SendInput::default()
                })
                .await?;
            if let Some(refusal) = sent.error {
                eprintln!("{refusal}"); // refused: the guard or limit says why
            } else if sent.state == STATE_HELD {
                eprintln!("held for {}", sent.approval_id); // a person decides; do not resend
            }
        }
    }
}
