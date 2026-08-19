pub mod ai;
pub mod card;
pub mod commands;
pub mod comment;
pub mod config;
pub mod db;
pub mod formatting;
pub mod poller;
pub mod strava;
pub mod types;

use log::{debug, info};
use std::sync::Arc;
use teloxide::prelude::*;

use commands::Command;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));
    dotenvy::dotenv().ok();

    let config = config::Config::from_env()?;
    debug!("Loaded config: {config:?}");

    // Open the database
    let db = Arc::new(db::Db::open(&config.database_path)?);
    info!("Database opened at {}", config.database_path);

    // Create Strava client
    let strava_client: Arc<dyn strava::StravaApi> = Arc::new(strava::StravaClient::new(
        config.strava_client_id.clone(),
        config.strava_client_secret.clone(),
    ));

    // AI client — None disables activity comments entirely
    let ai_client: Option<Arc<dyn ai::AiClient>> = if let Some(c) = &config.ai {
        info!("AI comments enabled (model {})", c.model);
        Some(Arc::new(ai::OpenAiCompatClient::new(
            c.base_url.clone(),
            c.model.clone(),
            c.api_key.clone(),
            std::time::Duration::from_secs(c.timeout_seconds),
        )?))
    } else {
        info!("AI comments disabled (AI_API_KEY not set)");
        None
    };

    // Build Telegram bot
    let bot = Bot::new(config.telegram_bot_token.clone());

    // Channel for commands → poller
    let (poll_tx, poll_rx) = tokio::sync::mpsc::unbounded_channel::<poller::PollCommand>();

    // App state for command handlers
    let app_state = Arc::new(commands::AppState {
        config: config.clone(),
        db: Arc::clone(&db),
        poll_tx,
        ai: ai_client.clone(),
        strava: Arc::clone(&strava_client),
    });

    // Command handler — commands go to handle_command; our commands with bad
    // arguments (which fail filter_command's typed parsing) get a usage reply;
    // everything else is silently ignored
    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(commands::handle_command),
        )
        .branch(Update::filter_message().endpoint(
            |bot: Bot, msg: Message, me: teloxide::types::Me| async move {
                if let Some(usage) = commands::usage_for(msg.text().unwrap_or(""), me.username()) {
                    bot.send_message(msg.chat.id, usage).await?;
                }
                Ok(())
            },
        ));

    let mut dispatcher = Dispatcher::builder(bot.clone(), handler)
        .dependencies(dptree::deps![app_state])
        .enable_ctrlc_handler()
        .build();

    // Spawn the poller in a background task
    tokio::spawn(poller::run_poll_loop(
        config.clone(),
        Arc::clone(&db),
        strava_client,
        bot.clone(),
        poll_rx,
        ai_client,
    ));

    // Verify connectivity and clear webhook
    let me = bot.get_me().await?;
    info!("Connected as @{}", me.username());
    bot.delete_webhook().drop_pending_updates(true).await?;
    info!("Webhook cleared, starting polling...");

    dispatcher.dispatch().await;

    Ok(())
}
