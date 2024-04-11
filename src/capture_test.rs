use crate::capture;
use crate::client::{ClientEvent, Position};
use crate::event::{Event, KeyboardEvent};
use anyhow::{anyhow, Result};
use futures::StreamExt;
use tokio::task::LocalSet;

pub fn run() -> Result<()> {
    log::info!("running input capture test");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    runtime.block_on(LocalSet::new().run_until(input_capture_test()))
}

async fn input_capture_test() -> Result<()> {
    log::info!("creating input capture");
    let mut input_capture = capture::create().await;
    log::info!("creating clients");
    input_capture.notify(ClientEvent::Create(0, Position::Left))?;
    input_capture.notify(ClientEvent::Create(1, Position::Right))?;
    input_capture.notify(ClientEvent::Create(2, Position::Top))?;
    input_capture.notify(ClientEvent::Create(3, Position::Bottom))?;
    loop {
        let (client, event) = input_capture
            .next()
            .await
            .ok_or(anyhow!("capture stream closed"))??;
        let pos = match client {
            0 => Position::Left,
            1 => Position::Right,
            2 => Position::Top,
            _ => Position::Bottom,
        };
        log::info!("position: {pos}, event: {event}");
        if let Event::Keyboard(KeyboardEvent::Key { key: 1, .. }) = event {
            input_capture.release()?;
        }
    }
}
