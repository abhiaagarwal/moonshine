use nvfbc::{BufferFormat, CudaCapturer};
use nvfbc::cuda::CaptureMethod;
use webserver::WebserverConfig;

use crate::encoder::{NvencEncoder, CodecType, VideoQuality};

mod cuda;
mod encoder;
mod error;
mod webserver;
mod service_publisher;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	let webserver_config = WebserverConfig {
		name: "Moonshine PC".to_string(),
		address: "localhost".to_string(),
		port: 47989,
		tls_port: 47984,
		cert: "./cert/cert.pem".into(),
		key: "./cert/key.pem".into(),
	};
	tokio::spawn(webserver::run(webserver_config.clone()));
	// tokio::spawn(service_publisher::run(47989));
	service_publisher::run(webserver_config.port).await;

	// let cuda_context = cuda::init_cuda(0)?;

	// // Create a capturer that captures to CUDA context.
	// let mut capturer = CudaCapturer::new()?;

	// let status = capturer.status()?;
	// println!("{:#?}", status);
	// if !status.can_create_now {
	// 	panic!("Can't create a CUDA capture session.");
	// }

	// let width = status.screen_size.w;
	// let height = status.screen_size.h;
	// let fps = 60;

	// capturer.start(BufferFormat::Bgra, fps)?;

	// let mut encoder = NvencEncoder::new(
	// 	width,
	// 	height,
	// 	CodecType::H264,
	// 	VideoQuality::Slowest,
	// 	cuda_context,
	// )?;

	// let start_time = std::time::Instant::now();
	// while start_time.elapsed().as_secs() < 20 {
	// 	let start = std::time::Instant::now();
	// 	let frame_info = capturer.next_frame(CaptureMethod::NoWaitIfNewFrame)?;
	// 	encoder.encode(frame_info.device_buffer, start_time.elapsed())?;
	// 	println!("Capture: {}msec", start.elapsed().as_millis());
	// }

	// encoder.stop()?;

	Ok(())
}
