use anyhow::{Context, Result};
use async_shutdown::{ShutdownManager, TriggerShutdownToken};
use enet::Enet;
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;

use super::{
	stream::{AudioStreamContext, VideoStreamContext},
	Session,
	SessionContext,
	SessionKeys,
};

pub enum SessionManagerCommand {
	SetStreamContext(VideoStreamContext, AudioStreamContext),
	GetSessionContext(oneshot::Sender<Option<SessionContext>>),
	InitializeSession(SessionContext),
	// GetCurrentSession(oneshot::Sender<Option<Session>>),
	StartSession,
	StopSession,
	UpdateKeys(SessionKeys),
}

#[derive(Clone)]
pub struct SessionManager {
	command_tx: mpsc::Sender<SessionManagerCommand>,
}

#[derive(Default)]
struct SessionManagerInner {
	/// The active session, or None if there is no active session.
	session: Option<Session>,

	/// The context within which the next video stream will be created.
	video_stream_context: Option<VideoStreamContext>,

	/// The context within which the next audio stream will be created.
	audio_stream_context: Option<AudioStreamContext>,
}

impl SessionManager {
	#[allow(clippy::result_unit_err)]
	pub fn new(config: Config, shutdown_token: TriggerShutdownToken<i32>) -> Result<Self> {
		// Preferably this gets constructed in control.rs, however it needs to stay
		// alive throughout the entire application runtime.
		// Once dropped, it cannot be initialized again.
		let enet = Enet::new().context("Failed to initialize Enet session")?;

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner: SessionManagerInner = Default::default();
		tokio::spawn(async move {
			inner.run(config, command_rx, enet).await;
			drop(shutdown_token);
		});
		Ok(Self { command_tx })
	}

	pub async fn set_stream_context(
		&self,
		video_stream_context: VideoStreamContext,
		audio_stream_context: AudioStreamContext,
	) -> Result<()> {
		self.command_tx
			.send(SessionManagerCommand::SetStreamContext(
				video_stream_context,
				audio_stream_context,
			))
			.await
			.context("Failed to send SetStreamContext command")
	}

	pub async fn get_session_context(&self) -> Result<Option<SessionContext>> {
		let (session_context_tx, session_context_rx) = oneshot::channel();
		self.command_tx
			.send(SessionManagerCommand::GetSessionContext(session_context_tx))
			.await
			.context("Failed to get session context")?;
		session_context_rx
			.await
			.context("Failed to wait for GetCurrentSession response")
	}

	pub async fn initialize_session(&self, context: SessionContext) -> Result<()> {
		self.command_tx
			.send(SessionManagerCommand::InitializeSession(context))
			.await
			.context("Failed to initialize session")?;
		Ok(())
	}

	// pub async fn current_session(&self) -> Result<Option<Session>, ()> {
	// 	let (session_tx, session_rx) = oneshot::channel();
	// 	self.command_tx.send(SessionManagerCommand::GetCurrentSession(session_tx))
	// 		.await
	// 		 .context("Failed to get current session")?;
	// 	session_rx.await
	// 		 .context("Failed to wait for GetCurrentSession response")?
	// }

	pub async fn start_session(&self) -> Result<()> {
		self.command_tx
			.send(SessionManagerCommand::StartSession)
			.await
			.context("Failed to start session")
	}

	pub async fn stop_session(&self) -> Result<()> {
		self.command_tx
			.send(SessionManagerCommand::StopSession)
			.await
			.context("Failed to stop session")
	}

	pub async fn update_keys(&self, keys: SessionKeys) -> Result<()> {
		self.command_tx
			.send(SessionManagerCommand::UpdateKeys(keys))
			.await
			.context("Failed to update keys")
	}
}

impl SessionManagerInner {
	async fn run(mut self, config: Config, mut command_rx: mpsc::Receiver<SessionManagerCommand>, enet: Enet) {
		tracing::debug!("Waiting for commands.");

		let mut stop_signal = ShutdownManager::new();

		loop {
			tokio::select! {
				_ = stop_signal.wait_shutdown_triggered() => {
					tracing::debug!("Closing session.");
					self.session = None;
					stop_signal = ShutdownManager::new();
				},

				command = command_rx.recv() => {
					let command = match command {
						Some(command) => command,
						None => {
							tracing::debug!("Command channel closed.");
							break;
						}
					};

					match command {
						SessionManagerCommand::SetStreamContext(video_stream_context, audio_stream_context) =>  {
							if self.session.is_none() {
								// Well we can, but it is not expected.
								tracing::warn!("Can't set stream context without an active session.");
								continue;
							}

							self.video_stream_context = Some(video_stream_context);
							self.audio_stream_context = Some(audio_stream_context);
						},

						SessionManagerCommand::GetSessionContext(session_context_tx) => {
							let context = self.session.as_ref().map(|s| Some(s.get_context().clone())).unwrap_or(None);
							if session_context_tx.send(context).is_err() {
								tracing::error!("Failed to send current session context.");
							}
						},

						SessionManagerCommand::InitializeSession(session_context) => {
							if self.session.is_some() {
								tracing::warn!("Can't initialize a session, there is already an active session.");
								continue;
							}

							self.session = match Session::new(config.clone(), session_context, enet.clone(), stop_signal.clone()) {
								Ok(session) => Some(session),
								Err(e) => {
									tracing::error!("Failed to create a new session: {e}");
									continue;
								},
							};
						},

						// SessionManagerCommand::GetCurrentSession(session_tx) => {
						// 	if session_tx.send(self.session.clone()).is_err() {
						// 		tracing::error!("Failed to send current session.");
						// 	}
						// }

						SessionManagerCommand::StartSession => {
							let Some(session) = &mut self.session else {
								tracing::warn!("Can't launch a session, there is no session created yet.");
								continue;
							};

							if session.is_running() {
								tracing::info!("Can't start session, it is already running.");
								continue;
							}

							let Some(video_stream_context) = self.video_stream_context.clone() else {
								tracing::warn!("Can't start a stream without a video stream context.");
								continue;
							};
							let Some(audio_stream_context) = self.audio_stream_context.clone() else {
								tracing::warn!("Can't start a stream without a audio stream context.");
								continue;
							};

							let _ = session.start_stream(video_stream_context, audio_stream_context).await;
						},

						SessionManagerCommand::StopSession => {
							if let Some(session) = &mut self.session {
								let _ = session.stop_stream().await;
								self.session = None;
							} else {
								tracing::debug!("Trying to stop session, but no session is currently active.");
							}
						},

						SessionManagerCommand::UpdateKeys(keys) => {
							let Some(session) = &mut self.session else {
								tracing::warn!("Can't update session keys, there is no session created yet.");
								continue;
							};

							let _ = session.update_keys(keys).await;
						},
					};
				}
			}
		}
	}
}
