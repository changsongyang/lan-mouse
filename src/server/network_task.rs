use std::{io, net::SocketAddr};

use anyhow::Result;
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::mpsc::{Receiver, Sender},
    task::JoinHandle,
};

use crate::frontend::FrontendEvent;
use input_event::{Event, ProtocolError};

use super::Server;

pub async fn new(
    server: Server,
    frontend_notify_tx: Sender<FrontendEvent>,
) -> Result<(
    JoinHandle<()>,
    Sender<(Event, SocketAddr)>,
    Receiver<Result<(Event, SocketAddr), NetworkError>>,
    Sender<u16>,
)> {
    // bind the udp socket
    let listen_addr = SocketAddr::new("0.0.0.0".parse().unwrap(), server.port.get());
    let mut socket = UdpSocket::bind(listen_addr).await?;
    let (receiver_tx, receiver_rx) = tokio::sync::mpsc::channel(32);
    let (sender_tx, sender_rx) = tokio::sync::mpsc::channel(32);
    let (port_tx, mut port_rx) = tokio::sync::mpsc::channel(32);

    let udp_task = tokio::task::spawn_local(async move {
        let mut sender_rx = sender_rx;
        loop {
            let udp_receiver = udp_receiver(&socket, &receiver_tx);
            let udp_sender = udp_sender(&socket, &mut sender_rx);
            tokio::select! {
                _ = udp_receiver => { }
                _ = udp_sender => { }
                port = port_rx.recv() => {
                    let Some(port) = port else {
                        break;
                    };

                    if socket.local_addr().unwrap().port() == port {
                        continue;
                    }

                    let listen_addr = SocketAddr::new("0.0.0.0".parse().unwrap(), port);
                    match UdpSocket::bind(listen_addr).await {
                        Ok(new_socket) => {
                            socket = new_socket;
                            server.port.replace(port);
                            let _ = frontend_notify_tx.send(FrontendEvent::PortChanged(port, None)).await;
                        }
                        Err(e) => {
                            log::warn!("could not change port: {e}");
                            let port = socket.local_addr().unwrap().port();
                            let _ = frontend_notify_tx.send(FrontendEvent::PortChanged(
                                    port,
                                    Some(format!("could not change port: {e}")),
                                )).await;
                        }
                    }
                }
            }
        }
    });
    Ok((udp_task, sender_tx, receiver_rx, port_tx))
}

async fn udp_receiver(
    socket: &UdpSocket,
    receiver_tx: &Sender<Result<(Event, SocketAddr), NetworkError>>,
) {
    loop {
        let event = receive_event(&socket).await;
        let _ = receiver_tx.send(event).await;
    }
}

async fn udp_sender(socket: &UdpSocket, rx: &mut Receiver<(Event, SocketAddr)>) {
    loop {
        let (event, addr) = match rx.recv().await {
            Some(e) => e,
            None => return,
        };
        if let Err(e) = send_event(&socket, event, addr) {
            log::warn!("udp send failed: {e}");
        };
    }
}

#[derive(Debug, Error)]
pub(crate) enum NetworkError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("network error: `{0}`")]
    Io(#[from] io::Error),
}

async fn receive_event(socket: &UdpSocket) -> Result<(Event, SocketAddr), NetworkError> {
    let mut buf = vec![0u8; 22];
    let (_amt, src) = socket.recv_from(&mut buf).await?;
    Ok((Event::try_from(buf)?, src))
}

fn send_event(sock: &UdpSocket, e: Event, addr: SocketAddr) -> Result<usize> {
    log::trace!("{:20} ------>->->-> {addr}", e.to_string());
    let data: Vec<u8> = (&e).into();
    // When udp blocks, we dont want to block the event loop.
    // Dropping events is better than potentially crashing the input capture.
    Ok(sock.try_send_to(&data, addr)?)
}
