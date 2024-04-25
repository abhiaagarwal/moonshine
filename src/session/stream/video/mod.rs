use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_shutdown::ShutdownManager;
use ffmpeg::{format::Pixel, Frame};
use tokio::{
	net::UdpSocket,
	sync::mpsc::{self, Sender},
};

use crate::{
	config::Config,
	ffmpeg::{check_ret, hwframe::HwFrameContext},
};

mod capture;
use capture::FrameCapturer;

mod encoder;
use encoder::Encoder;

#[derive(Debug)]
enum VideoStreamCommand {
	Start,
	RequestIdrFrame,
}

#[derive(Clone, Debug, Default)]
pub struct VideoStreamContext {
	pub width: u32,
	pub height: u32,
	pub fps: u32,
	pub packet_size: usize,
	pub bitrate: usize,
	pub minimum_fec_packets: u32,
	pub qos: bool,
	pub video_format: u32,
}

#[derive(Clone)]
pub struct VideoStream {
	command_tx: Sender<VideoStreamCommand>,
}

struct VideoStreamInner {}

impl VideoStream {
	pub fn new(config: Config, context: VideoStreamContext, stop_signal: ShutdownManager<()>) -> Self {
		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = VideoStreamInner {};
		tokio::spawn(stop_signal.wrap_cancel(
			stop_signal.wrap_trigger_shutdown((), inner.run(config, context, command_rx, stop_signal.clone())),
		));

		Self { command_tx }
	}

	pub async fn start(&self) -> Result<()> {
		self.command_tx
			.send(VideoStreamCommand::Start)
			.await
			.context("Failed to send Start command")
	}

	pub async fn request_idr_frame(&self) -> Result<()> {
		self.command_tx
			.send(VideoStreamCommand::RequestIdrFrame)
			.await
			.context("Failed to send RequestIdrFrame command")
	}
}

impl VideoStreamInner {
	async fn run(
		self,
		config: Config,
		mut context: VideoStreamContext,
		mut command_rx: mpsc::Receiver<VideoStreamCommand>,
		stop_signal: ShutdownManager<()>,
	) -> Result<()> {
		let socket = UdpSocket::bind((config.address, config.stream.video.port))
			.await
			.context("Failed to bind to UDP socket")?;

		if context.qos {
			// TODO: Check this value 160, what does it mean exactly?
			tracing::debug!("Enabling QoS on video socket.");
			socket.set_tos(160).context("Failed to set QoS on the video socket")?;
		}

		tracing::debug!(
			"Listening for video messages on {}",
			socket
				.local_addr()
				.context("Failed to get local address associated with control socket")?
		);

		let (packet_tx, mut packet_rx) = mpsc::channel::<Vec<u8>>(1024);
		tokio::spawn(async move {
			let mut buf = [0; 1024];
			let mut client_address = None;

			loop {
				tokio::select! {
					packet = packet_rx.recv() => {
						match packet {
							Some(packet) => {
								if let Some(client_address) = client_address {
									if let Err(e) = socket.send_to(packet.as_slice(), client_address).await {
										tracing::warn!("Failed to send packet to client: {e}");
									}
								}
							},
							None => {
								tracing::debug!("Packet channel closed.");
								break;
							},
						}
					},

					message = socket.recv_from(&mut buf) => {
						let (len, address) = match message {
							Ok((len, address)) => (len, address),
							Err(e) => {
								tracing::warn!("Failed to receive message: {e}");
								break;
							},
						};

						if &buf[..len] == b"PING" {
							tracing::trace!("Received video stream PING message from {address}.");
							client_address = Some(address);
						} else {
							tracing::warn!("Received unknown message on video stream of length {len}.");
						}
					},
				}
			}

			tracing::debug!("Stopping video stream.");
		});

		let mut started_streaming = false;
		let (idr_frame_request_tx, _idr_frame_request_rx) = tokio::sync::broadcast::channel(1);
		while let Some(command) = command_rx.recv().await {
			match command {
				VideoStreamCommand::RequestIdrFrame => {
					tracing::info!("Received request for IDR frame, next frame will be an IDR frame.");
					idr_frame_request_tx
						.send(())
						.context("Failed to send IDR frame request to encoder")?;
				},
				VideoStreamCommand::Start => {
					if started_streaming {
						tracing::warn!("Can't start streaming twice.");
						continue;
					}

					// TODO: Make the GPU index configurable.
					let cuda_device = cudarc::driver::CudaDevice::new(0).context("Failed to initialize CUDA")?;

					let capturer = FrameCapturer::new()?;
					let status = capturer.status()?;
					if status.screen_size.w != context.width || status.screen_size.h != context.height {
						// TODO: Resize the CUDA buffer to the requested size?
						tracing::warn!(
							"Client asked for resolution {}x{}, but we are generating a resolution of {}x{}.",
							context.width,
							context.height,
							status.screen_size.w,
							status.screen_size.h
						);
						context.width = status.screen_size.w;
						context.height = status.screen_size.h;
					}

					let mut encoder = Encoder::new(
						&cuda_device,
						if context.video_format == 0 {
							&config.stream.video.codec_h264
						} else {
							&config.stream.video.codec_hevc
						},
						context.width,
						context.height,
						context.fps,
						context.bitrate,
					)?;

					let capture_buffer = create_frame(
						context.width,
						context.height,
						Pixel::CUDA,
						&mut encoder.hw_frame_context,
					)?;
					let intermediate_buffer = Arc::new(Mutex::new(create_frame(
						context.width,
						context.height,
						Pixel::CUDA,
						&mut encoder.hw_frame_context,
					)?));
					let encoder_buffer = create_frame(
						context.width,
						context.height,
						Pixel::CUDA,
						&mut encoder.hw_frame_context,
					)?;
					let notifier = Arc::new(std::sync::Condvar::new());

					let capture_thread = std::thread::Builder::new().name("video-capture".to_string()).spawn({
						let intermediate_buffer = intermediate_buffer.clone();
						let notifier = notifier.clone();
						let context = context.clone();
						let stop_signal = stop_signal.clone();
						move || {
							cuda_device
								.bind_to_thread()
								.context("Failed to bind CUDA device to thread")?;
							capturer.run(context.fps, capture_buffer, intermediate_buffer, notifier, stop_signal)
						}
					});
					if let Err(e) = capture_thread {
						tracing::error!("Failed to start video capture thread: {e}");
						continue;
					}

					let encode_thread = std::thread::Builder::new().name("video-encode".to_string()).spawn({
						let packet_tx = packet_tx.clone();
						let notifier = notifier.clone();
						let idr_frame_request_rx = idr_frame_request_tx.subscribe();
						let context = context.clone();
						let stop_signal = stop_signal.clone();
						move || {
							encoder.run(
								packet_tx,
								idr_frame_request_rx,
								context.packet_size,
								context.minimum_fec_packets,
								config.stream.video.fec_percentage,
								encoder_buffer,
								intermediate_buffer,
								notifier,
								stop_signal,
							)
						}
					});
					if let Err(e) = encode_thread {
						tracing::error!("Failed to create video encoding thread: {e}");
						continue;
					}

					started_streaming = true;
				},
			}
		}

		tracing::debug!("Command channel closed.");
		Ok(())
	}
}

fn create_frame(width: u32, height: u32, pixel_format: Pixel, context: &mut HwFrameContext) -> Result<Frame> {
	unsafe {
		let mut frame = Frame::empty();
		(*frame.as_mut_ptr()).format = pixel_format as i32;
		(*frame.as_mut_ptr()).width = width as i32;
		(*frame.as_mut_ptr()).height = height as i32;
		(*frame.as_mut_ptr()).hw_frames_ctx = context.as_raw_mut();

		check_ret(ffmpeg::sys::av_hwframe_get_buffer(
			context.as_raw_mut(),
			frame.as_mut_ptr(),
			0,
		))
		.context("Failed to create CUDA frame")?;
		check_ret(ffmpeg::sys::av_hwframe_get_buffer(
			context.as_raw_mut(),
			frame.as_mut_ptr(),
			0,
		))
		.context("Failed to allocate hardware frame")?;
		(*frame.as_mut_ptr()).linesize[0] = (*frame.as_ptr()).width * 4;

		Ok(frame)
	}
}
