//! Each invocation of Clap provider is a session. When you exit the provider, the session ends.

use crate::stdio_server::input::{
    InternalProviderEvent, PluginEvent, ProviderEvent, ProviderEventSender,
};
use crate::stdio_server::plugin::ClapPlugin;
use crate::stdio_server::provider::{ClapProvider, Context, ProviderSource};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt::Debug;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;

pub type ProviderSessionId = u64;

#[derive(Debug)]
pub struct ProviderSession {
    ctx: Context,
    provider_session_id: ProviderSessionId,
    /// Each provider session can have its own message processing logic.
    provider: Box<dyn ClapProvider>,
    provider_events: UnboundedReceiver<ProviderEvent>,
}

impl ProviderSession {
    pub fn new(
        ctx: Context,
        provider_session_id: ProviderSessionId,
        provider: Box<dyn ClapProvider>,
    ) -> (Self, UnboundedSender<ProviderEvent>) {
        let (provider_event_sender, provider_event_receiver) = unbounded_channel();

        let provider_session = ProviderSession {
            ctx,
            provider_session_id,
            provider,
            provider_events: provider_event_receiver,
        };

        (provider_session, provider_event_sender)
    }

    pub fn start_event_loop(self) {
        tracing::debug!(
            provider_session_id = self.provider_session_id,
            provider_id = %self.ctx.provider_id(),
            debounce = self.ctx.env.debounce,
            "Spawning a new provider session task",
        );

        tokio::spawn(async move {
            if self.ctx.env.debounce {
                self.run_event_loop_with_debounce().await;
            } else {
                self.run_event_loop_without_debounce().await;
            }
        });
    }

    async fn run_event_loop_with_debounce(mut self) {
        // https://github.com/denoland/deno/blob/1fb5858009f598ce3f917f9f49c466db81f4d9b0/cli/lsp/diagnostics.rs#L141
        //
        // Debounce timer delay. 150ms between keystrokes is about 45 WPM, so we
        // want something that is longer than that, but not too long to
        // introduce detectable UI delay; 200ms is a decent compromise.
        const DELAY: Duration = Duration::from_millis(200);
        // If the debounce timer isn't active, it will be set to expire "never",
        // which is actually just 1 year in the future.
        const NEVER: Duration = Duration::from_secs(365 * 24 * 60 * 60);

        let mut on_move_dirty = false;
        let on_move_delay = Duration::from_millis(50);
        let on_move_timer = tokio::time::sleep(NEVER);
        tokio::pin!(on_move_timer);

        let mut on_typed_dirty = false;
        // Delay can be adjusted once we know the provider source scale.
        //
        // Here is the benchmark result of filtering on AMD 5900X:
        //
        // |    Type     |  1k   |  10k   | 100k  |
        // |    ----     |  ---- | ----   | ----  |
        // |     filter  | 413us | 12ms   | 75ms  |
        // | par_filter  | 327us |  3ms   | 20ms  |
        let mut on_typed_delay = DELAY;
        let on_typed_timer = tokio::time::sleep(NEVER);
        tokio::pin!(on_typed_timer);

        loop {
            tokio::select! {
                maybe_event = self.provider_events.recv() => {
                    match maybe_event {
                        Some(event) => {
                            tracing::trace!("[with_debounce] Received event: {event:?}");

                            match event {
                                ProviderEvent::NewSession => unreachable!(),
                                ProviderEvent::Internal(internal_event) => {
                                    match internal_event {
                                        InternalProviderEvent::Terminate => {
                                            self.provider.on_terminate(&mut self.ctx, self.provider_session_id);
                                            break;
                                        }
                                        InternalProviderEvent::OnInitialize => {
                                            match self.provider.on_initialize(&mut self.ctx).await {
                                                Ok(()) => {
                                                    // Set a smaller debounce if the source scale is small.
                                                    if let ProviderSource::Small { total, .. } = *self
                                                        .ctx
                                                        .provider_source
                                                        .read()
                                                    {
                                                        if total < 10_000 {
                                                            on_typed_delay = Duration::from_millis(10);
                                                        } else if total < 100_000 {
                                                            on_typed_delay = Duration::from_millis(50);
                                                        } else if total < 200_000 {
                                                            on_typed_delay = Duration::from_millis(100);
                                                        }
                                                    }
                                                    // Try to fulfill the preview window
                                                    if let Err(err) = self.provider.on_move(&mut self.ctx).await {
                                                        tracing::debug!(?err, "Failed to preview after on_initialize completed");
                                                    }
                                                }
                                                Err(err) => {
                                                    tracing::error!(?err, "Failed to process {internal_event:?}");
                                                }
                                            }
                                        }
                                    }
                                }
                                ProviderEvent::Exit => {
                                    self.provider.on_terminate(&mut self.ctx, self.provider_session_id);
                                    break;
                                }
                                ProviderEvent::OnMove => {
                                    on_move_dirty = true;
                                    on_move_timer.as_mut().reset(Instant::now() + on_move_delay);
                                }
                                ProviderEvent::OnTyped => {
                                    on_typed_dirty = true;
                                    on_typed_timer.as_mut().reset(Instant::now() + on_typed_delay);
                                }
                                ProviderEvent::Key(key_event) => {
                                    if let Err(err) = self.provider.on_key_event(&mut self.ctx, key_event).await {
                                        tracing::error!(?err, "Failed to process {event:?}");
                                    }
                                }
                            }
                          }
                          None => break, // channel has closed.
                      }
                }
                _ = on_move_timer.as_mut(), if on_move_dirty => {
                    on_move_dirty = false;
                    on_move_timer.as_mut().reset(Instant::now() + NEVER);

                    if let Err(err) = self.provider.on_move(&mut self.ctx).await {
                        tracing::error!(?err, "Failed to process ProviderEvent::OnMove");
                    }
                }
                _ = on_typed_timer.as_mut(), if on_typed_dirty => {
                    on_typed_dirty = false;
                    on_typed_timer.as_mut().reset(Instant::now() + NEVER);

                    let _ = self.ctx.record_input().await;

                    if let Err(err) = self.provider.on_typed(&mut self.ctx).await {
                        tracing::error!(?err, "Failed to process ProviderEvent::OnTyped");
                    }

                    let _ = self.provider.on_move(&mut self.ctx).await;
                }
            }
        }
    }

    async fn run_event_loop_without_debounce(mut self) {
        while let Some(event) = self.provider_events.recv().await {
            tracing::trace!("[without_debounce] Received event: {event:?}");

            match event {
                ProviderEvent::NewSession => unreachable!(),
                ProviderEvent::Internal(internal_event) => {
                    match internal_event {
                        InternalProviderEvent::OnInitialize => {
                            if let Err(err) = self.provider.on_initialize(&mut self.ctx).await {
                                tracing::error!(?err, "Failed at process {internal_event:?}");
                                continue;
                            }
                            // Try to fulfill the preview window
                            if let Err(err) = self.provider.on_move(&mut self.ctx).await {
                                tracing::debug!(
                                    ?err,
                                    "Failed to preview after on_initialize completed"
                                );
                            }
                        }
                        InternalProviderEvent::Terminate => {
                            self.provider
                                .on_terminate(&mut self.ctx, self.provider_session_id);
                            break;
                        }
                    }
                }
                ProviderEvent::Exit => {
                    self.provider
                        .on_terminate(&mut self.ctx, self.provider_session_id);
                    break;
                }
                ProviderEvent::OnMove => {
                    if let Err(err) = self.provider.on_move(&mut self.ctx).await {
                        tracing::debug!(?err, "Failed to process {event:?}");
                    }
                }
                ProviderEvent::OnTyped => {
                    let _ = self.ctx.record_input().await;
                    if let Err(err) = self.provider.on_typed(&mut self.ctx).await {
                        tracing::debug!(?err, "Failed to process {event:?}");
                    }
                }
                ProviderEvent::Key(key_event) => {
                    if let Err(err) = self.provider.on_key_event(&mut self.ctx, key_event).await {
                        tracing::error!(?err, "Failed to process {key_event:?}");
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct PluginSession {
    plugin: Box<dyn ClapPlugin>,
    event_delay: Duration,
    plugin_events: UnboundedReceiver<PluginEvent>,
}

impl PluginSession {
    pub fn create(
        plugin: Box<dyn ClapPlugin>,
        event_delay: Duration,
    ) -> UnboundedSender<PluginEvent> {
        let (plugin_event_sender, plugin_event_receiver) = unbounded_channel();

        let plugin_session = PluginSession {
            plugin,
            event_delay,
            plugin_events: plugin_event_receiver,
        };

        plugin_session.start_event_loop();

        plugin_event_sender
    }

    fn start_event_loop(mut self) {
        tracing::debug!("Spawning a new plugin session task");

        tokio::spawn(async move {
            // If the debounce timer isn't active, it will be set to expire "never",
            // which is actually just 1 year in the future.
            const NEVER: Duration = Duration::from_secs(365 * 24 * 60 * 60);

            let mut pending_autocmd = None;
            let mut notification_dirty = false;
            let notification_timer = tokio::time::sleep(NEVER);
            tokio::pin!(notification_timer);

            loop {
                tokio::select! {
                    maybe_plugin_event = self.plugin_events.recv() => {
                        match maybe_plugin_event {
                            Some(plugin_event) => {
                                match plugin_event {
                                    PluginEvent::Autocmd(autocmd) => {
                                        pending_autocmd.replace(autocmd);
                                        notification_dirty = true;
                                        notification_timer
                                            .as_mut()
                                            .reset(Instant::now() + self.event_delay);
                                    }
                                }
                            }
                            None => break, // channel has closed.
                        }
                    }
                    _ = notification_timer.as_mut(), if notification_dirty => {
                        notification_dirty = false;
                        notification_timer.as_mut().reset(Instant::now() + NEVER);

                        if let Some(autocmd) = pending_autocmd.take() {
                            if let Err(err) = self.plugin.on_autocmd(autocmd).await {
                                tracing::error!(?err, "Failed at process {autocmd:?}");
                            }
                        }
                    }
                }
            }
        });
    }
}

/// This structs manages all the created sessions.
///
/// A plugin is a general service, a provider is a specialized plugin
/// which is dedicated to provide the filtering service.
#[derive(Debug, Default)]
pub struct ServiceManager {
    providers: HashMap<ProviderSessionId, ProviderEventSender>,
    plugins: Vec<UnboundedSender<PluginEvent>>,
}

impl ServiceManager {
    /// Creates a new provider session if `provider_session_id` does not exist.
    pub fn new_provider(
        &mut self,
        provider_session_id: ProviderSessionId,
        provider: Box<dyn ClapProvider>,
        ctx: Context,
    ) {
        for (provider_session_id, sender) in self.providers.drain() {
            tracing::debug!(?provider_session_id, "Sending internal Terminate signal");
            sender.send(ProviderEvent::Internal(InternalProviderEvent::Terminate));
        }

        if let Entry::Vacant(v) = self.providers.entry(provider_session_id) {
            let (provider_session, provider_event_sender) =
                ProviderSession::new(ctx, provider_session_id, provider);
            provider_session.start_event_loop();

            provider_event_sender
                .send(ProviderEvent::Internal(InternalProviderEvent::OnInitialize))
                .expect("Failed to send ProviderEvent::OnInitialize");

            v.insert(ProviderEventSender::new(
                provider_event_sender,
                provider_session_id,
            ));
        } else {
            tracing::error!(
                provider_session_id,
                "Skipped as given provider session already exists"
            );
        }
    }

    /// Creates a new plugin session with the default debounce setting.
    pub fn new_plugin(&mut self, plugin: Box<dyn ClapPlugin>) {
        self.plugins
            .push(PluginSession::create(plugin, Duration::from_millis(50)));
    }

    pub fn notify_plugins(&mut self, plugin_event: PluginEvent) {
        self.plugins
            .retain(|plugin_sender| plugin_sender.send(plugin_event.clone()).is_ok())
    }

    pub fn exists(&self, provider_session_id: ProviderSessionId) -> bool {
        self.providers.contains_key(&provider_session_id)
    }

    pub fn try_exit(&mut self, provider_session_id: ProviderSessionId) {
        if self.exists(provider_session_id) {
            self.notify_provider_exit(provider_session_id);
        }
    }

    /// Dispatch the session event to the background session task accordingly.
    pub fn notify_provider(&self, provider_session_id: ProviderSessionId, event: ProviderEvent) {
        if let Some(sender) = self.providers.get(&provider_session_id) {
            sender.send(event);
        } else {
            tracing::error!(
                provider_session_id,
                sessions = ?self.providers.keys(),
                "Couldn't find the sender for given session",
            );
        }
    }

    /// Stop the session task by sending [`ProviderEvent::Exit`].
    pub fn notify_provider_exit(&mut self, provider_session_id: ProviderSessionId) {
        if let Some(sender) = self.providers.remove(&provider_session_id) {
            sender.send(ProviderEvent::Exit);
        }
    }
}
