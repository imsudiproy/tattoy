//! Animate the cursor using shaders.

use color_eyre::eyre::{ContextCompat as _, Result};
use futures_util::FutureExt as _;

use crate::tattoys::tattoyer::Tattoyer;

/// The size of the cursor in units of terminal UTF8 half blocl "pixels".
pub const CURSOR_DIMENSIONS_REAL: (f32, f32) = (1.0, 2.0);

/// All the user config for the shader tattoy.
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(default)]
pub(crate) struct Config {
    /// Enable/disable the shaders on and off
    pub enabled: bool,
    /// The path to a given GLSL shader file.
    pub path: std::path::PathBuf,
    /// The opacity of the rendered shader layer.
    pub opacity: f32,
    /// The layer (or z-index) into which the shaders are rendered.
    pub layer: i16,
    /// Whether to upload a pixel representation of the user's terminal. Useful for shader's that
    /// replace the text of the terminal, as Ghostty shaders do.
    pub upload_tty_as_pixels: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            path: format!(
                "{}/{}",
                crate::config::main::CURSOR_SHADER_DIRECTORY_NAME,
                crate::config::main::DEFAULT_CURSOR_SHADER_FILENAME
            )
            .into(),
            opacity: 0.75,
            layer: -1,
            upload_tty_as_pixels: false,
        }
    }
}

/// `AnimatedCursor`
pub(crate) struct AnimatedCursor<'shaders> {
    /// The base Tattoy struct
    tattoy: Tattoyer,
    /// All the special GPU handling code.
    gpu: super::gpu::pipeline::GPU<'shaders>,
}

impl AnimatedCursor<'_> {
    /// Instantiate
    async fn new(
        output_channel: tokio::sync::mpsc::Sender<crate::run::FrameUpdate>,
        state: std::sync::Arc<crate::shared_state::SharedState>,
    ) -> Result<Self> {
        let shader_directory = state.config_path.read().await.clone();
        let shader_path = state.config.read().await.animated_cursor.path.clone();
        let tty_size = *state.tty_size.read().await;
        let gpu = super::gpu::pipeline::GPU::new(
            shader_directory.join(shader_path),
            tty_size.width,
            tty_size.height * 2,
            state.protocol_tx.clone(),
        )
        .await?;
        let layer = state.config.read().await.animated_cursor.layer;
        let opacity = state.config.read().await.animated_cursor.opacity;
        let tattoy = Tattoyer::new(
            "animated_cursor".to_owned(),
            state,
            layer,
            opacity,
            output_channel,
        )
        .await;
        Ok(Self { tattoy, gpu })
    }

    /// Our main entrypoint.
    pub(crate) async fn start(
        output: tokio::sync::mpsc::Sender<crate::run::FrameUpdate>,
        state: std::sync::Arc<crate::shared_state::SharedState>,
    ) -> Result<()> {
        let may_panic = std::panic::AssertUnwindSafe(async {
            let result = Self::main(output, &state).await;

            if let Err(error) = result {
                tracing::error!("GPU pipeline error: {error:?}");
                state
                    .send_notification(
                        "GPU pipeline error",
                        crate::tattoys::notifications::message::Level::Error,
                        Some(error.root_cause().to_string()),
                        true,
                    )
                    .await;
                Err(error)
            } else {
                Ok(())
            }
        });

        if let Err(error) = may_panic.catch_unwind().await {
            let message = if let Some(message) = error.downcast_ref::<String>() {
                message
            } else if let Some(message) = error.downcast_ref::<&str>() {
                message
            } else {
                "Caught a panic with an unknown type."
            };
            tracing::error!("Shader panic: {message:?}");
            state
                .send_notification(
                    "GPU pipeline panic",
                    crate::tattoys::notifications::message::Level::Error,
                    Some(message.into()),
                    true,
                )
                .await;
        }

        Ok(())
    }

    /// Enter the main render loop. We put it in its own function so that we can easily handle any
    /// errors.
    async fn main(
        output: tokio::sync::mpsc::Sender<crate::run::FrameUpdate>,
        state: &std::sync::Arc<crate::shared_state::SharedState>,
    ) -> Result<()> {
        let mut protocol = state.protocol_tx.subscribe();
        let mut animated_cursor = Self::new(output, std::sync::Arc::clone(state)).await?;

        #[expect(
            clippy::integer_division_remainder_used,
            reason = "This is caused by the `tokio::select!`"
        )]
        loop {
            tokio::select! {
                () = animated_cursor.tattoy.sleep_until_next_frame_tick() => {
                    animated_cursor.render().await?;
                },
                result = protocol.recv() => {
                    if matches!(result, Ok(crate::run::Protocol::End)) {
                        break;
                    }
                    animated_cursor.handle_protocol_message(result).await?;
                }
            }
        }

        Ok(())
    }

    /// Handle messages from the main Tattoy app.
    async fn handle_protocol_message(
        &mut self,
        protocol_result: std::result::Result<
            crate::run::Protocol,
            tokio::sync::broadcast::error::RecvError,
        >,
    ) -> Result<()> {
        match protocol_result {
            Ok(message) => {
                if matches!(&message, crate::run::Protocol::Repaint) {
                    self.upload_tty_as_pixels().await?;
                }

                self.gpu.handle_protocol_message(&message).await?;
                self.tattoy.handle_common_protocol_messages(message)?;
            }
            Err(error) => tracing::error!("Receiving protocol message: {error:?}"),
        }

        Ok(())
    }

    /// Upload the TTY content as coloured pixels.
    async fn upload_tty_as_pixels(&mut self) -> Result<()> {
        let is_upload_tty_as_pixels = self
            .tattoy
            .state
            .config
            .read()
            .await
            .animated_cursor
            .upload_tty_as_pixels;
        let image = self
            .tattoy
            .get_tty_image_for_upload(is_upload_tty_as_pixels)?;
        self.gpu.update_ichannel_texture_data(&image);

        Ok(())
    }

    /// Tick the render
    async fn render(&mut self) -> Result<()> {
        let cursor = self.tattoy.screen.surface.cursor_position();
        self.gpu
            .update_cursor_position(cursor.0.try_into()?, cursor.1.try_into()?);

        let config = self
            .tattoy
            .state
            .config
            .read()
            .await
            .animated_cursor
            .clone();
        self.tattoy.initialise_surface();
        self.tattoy.opacity = config.opacity;
        self.tattoy.layer = config.layer;

        let image = self.gpu.render().await?;

        let tty_height_in_pixels = u32::from(self.tattoy.height) * 2;
        for y in 0..tty_height_in_pixels {
            for x in 0..self.tattoy.width {
                let offset_for_reversal = 1;
                let y_reversed = tty_height_in_pixels - y - offset_for_reversal;
                let pixel = image
                    .get_pixel_checked(x.into(), y_reversed)
                    .context(format!("Couldn't get pixel: {x}x{y_reversed}"))?
                    .0;
                self.tattoy
                    .surface
                    .add_pixel(x.into(), y.try_into()?, pixel.into())?;
            }
        }

        self.tattoy.send_output().await?;

        Ok(())
    }
}
