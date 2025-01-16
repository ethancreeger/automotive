//! Async wrapper for Adapters implementing the [`CanAdapter`] trait.

use std::collections::{HashMap, VecDeque};

use crate::can::CanAdapter;
use crate::can::Frame;
use crate::can::Identifier;
use crate::Stream;
use async_stream::stream;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::debug;

const CAN_TX_BUFFER_SIZE: usize = 128;
const CAN_RX_BUFFER_SIZE: usize = 1024;
const COMMAND_BUFFER_SIZE: usize = 10;
const DEBUG: bool = false;

type BusIdentifier = (u8, Identifier);
type FrameCallback = (Frame, oneshot::Sender<()>);

enum Command {
    BaudRate(u16),
    Power(bool),
}

fn process<T: CanAdapter>(
    mut adapter: T,
    mut shutdown_receiver: oneshot::Receiver<()>,
    mut cmd_receiver: mpsc::Receiver<(Command, oneshot::Sender<()>)>,
    rx_sender: broadcast::Sender<Frame>,
    mut tx_receiver: mpsc::Receiver<(Frame, oneshot::Sender<()>)>,
) {
    let mut buffer: VecDeque<Frame> = VecDeque::new();
    let mut callbacks: HashMap<BusIdentifier, VecDeque<FrameCallback>> = HashMap::new();

    while shutdown_receiver.try_recv().is_err() {
        let frames: Vec<Frame> = adapter.recv().expect("Failed to Receive CAN Frames");

        for frame in frames {
            if DEBUG {
                debug! {"RX {:?}", frame};
            }

            // Wake up sender
            if frame.loopback {
                let callback = callbacks
                    .entry((frame.bus, frame.id))
                    .or_default()
                    .pop_front();

                match callback {
                    Some((tx_frame, callback)) => {
                        // Ensure the frame we received matches the frame belonging to the callback.
                        // If not, we have a bug in the adapter implementation and frames are sent/received out of order.
                        assert_eq!(tx_frame, frame);

                        // Callback might be dropped if the sender is not waiting for the response
                        callback.send(()).ok();
                    }
                    None => panic!("Received loopback frame with no pending callback"),
                };
            }

            rx_sender.send(frame).unwrap();
        }

        // TODO: use poll_recv_many?
        while let Ok((frame, callback)) = tx_receiver.try_recv() {
            let mut loopback_frame = frame.clone();
            loopback_frame.loopback = true;

            // Insert callback into hashmap
            callbacks
                .entry((frame.bus, frame.id))
                .or_default()
                .push_back((loopback_frame, callback));

            if DEBUG {
                debug! {"TX {:?}", frame};
            }

            buffer.push_back(frame);
        }
        if !buffer.is_empty() {
            adapter.send(&mut buffer).unwrap();

            if !buffer.is_empty() {
                debug!(
                    "Failed to send all frames, requeueing {} frames",
                    buffer.len()
                );
            }
        }

        while let Ok((cmd, callback)) = cmd_receiver.try_recv() {
            match cmd {
                Command::BaudRate(baud) => callback.send(
                    adapter.baud_rate(baud).expect("Failed to set CAN baud rate")
                ).ok(),
                Command::Power(on) => callback.send(
                    adapter.power(on).expect("Failed to set ECU power")
                ).ok(),
            };
        }

        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Async wrapper around a [`CanAdapter`]. Starts a background thread to handle sending and receiving frames. Uses tokio channels to communicate with the background thread.
pub struct AsyncCanAdapter {
    processing_handle: Option<std::thread::JoinHandle<()>>,
    cmd_sender: mpsc::Sender<(Command, oneshot::Sender<()>)>,
    recv_receiver: broadcast::Receiver<Frame>,
    send_sender: mpsc::Sender<(Frame, oneshot::Sender<()>)>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl AsyncCanAdapter {
    pub fn new<T: CanAdapter + Send + Sync + 'static>(adapter: T) -> Self {
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let (cmd_sender, cmd_receiver) = mpsc::channel(COMMAND_BUFFER_SIZE);
        let (send_sender, send_receiver) = mpsc::channel(CAN_TX_BUFFER_SIZE);
        let (recv_sender, recv_receiver) = broadcast::channel(CAN_RX_BUFFER_SIZE);

        let mut ret = AsyncCanAdapter {
            shutdown: Some(shutdown_sender),
            processing_handle: None,
            cmd_sender,
            recv_receiver,
            send_sender,
        };

        ret.processing_handle = Some(std::thread::spawn(move || {
            process(adapter, shutdown_receiver, cmd_receiver, recv_sender, send_receiver);
        }));

        ret
    }

    pub async fn baud_rate(&mut self, baud_rate: u16) {
        let (callback_sender, callback_receiver) = oneshot::channel();
        self.cmd_sender
            .send((Command::BaudRate(baud_rate), callback_sender))
            .await
            .unwrap();

        callback_receiver.await.unwrap();
    }

    pub async fn power(&mut self, on: bool, hold: std::time::Duration) {
        let (callback_sender, callback_receiver) = oneshot::channel();
        self.cmd_sender
            .send((Command::Power(on), callback_sender))
            .await
            .unwrap();

        tokio::time::sleep(hold).await;

        callback_receiver.await.unwrap();
    }

    /// Receive all frames.
    pub fn recv(&self) -> impl Stream<Item = Frame> {
        self.recv_filter(|_| true)
    }

    /// Receive frames that match a filter. Useful in combination with stream adapters.
    pub fn recv_filter(&self, filter: impl Fn(&Frame) -> bool) -> impl Stream<Item = Frame> {
        let mut rx = self.recv_receiver.resubscribe();

        Box::pin(stream! {
            loop { match rx.recv().await {
                    Ok(frame) => {
                        if filter(&frame) {
                            yield frame
                        }
                    },
                    Err(RecvError::Closed) => panic!("Adapter thread has exited"),
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!("Receive too slow, dropping {} frame(s).", n)
                    },
                }
            }
        })
    }

    /// Send a single frame. The Future will resolve once the frame has been handed over to the adapter for sending. This does not mean the message is sent out on the CAN bus yet, as this could be pending arbitration.
    pub async fn send(&self, frame: &Frame) {
        // Create oneshot channel to signal the completion of the send operation
        let (callback_sender, callback_receiver) = oneshot::channel();
        self.send_sender
            .send((frame.clone(), callback_sender))
            .await
            .unwrap();

        callback_receiver.await.unwrap();
    }
}

impl Drop for AsyncCanAdapter {
    fn drop(&mut self) {
        if let Some(handle) = self.processing_handle.take() {
            // Send shutdown signal to background tread
            self.shutdown.take().unwrap().send(()).unwrap();
            handle.join().unwrap();
        }
    }
}
