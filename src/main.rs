pub mod card;
pub mod commands;
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

    // Create one Strava client per configured app slot.
    let strava_clients = Arc::new(strava::StravaClients {
        slot_1: Arc::new(strava::StravaClient::new(
            config.strava_apps.slot_1.id.clone(),
            config.strava_apps.slot_1.secret.clone(),
        )),
        slot_2: config.strava_apps.slot_2.as_ref().map(|app| {
            Arc::new(strava::StravaClient::new(
                app.id.clone(),
                app.secret.clone(),
            )) as Arc<dyn strava::StravaApi>
        }),
    });

    // Build Telegram bot
    let bot = Bot::new(config.telegram_bot_token.clone());

    // Channel for commands → poller
    let (poll_tx, poll_rx) = tokio::sync::mpsc::unbounded_channel::<poller::PollCommand>();

    // App state for command handlers
    let app_state = Arc::new(commands::AppState {
        config: config.clone(),
        db: Arc::clone(&db),
        strava_clients: Arc::clone(&strava_clients),
        poll_tx,
    });

    // Command handler — commands go to handle_command, everything else is silently ignored
    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(commands::handle_command),
        )
        .branch(
            Update::filter_message().endpoint(|_bot: Bot, _msg: Message| async move { Ok(()) }),
        );

    let mut dispatcher = Dispatcher::builder(bot.clone(), handler)
        .dependencies(dptree::deps![app_state])
        .enable_ctrlc_handler()
        .build();

    // Spawn the poller in a background task
    tokio::spawn(poller::run_poll_loop(
        config.clone(),
        Arc::clone(&db),
        strava_clients,
        bot.clone(),
        poll_rx,
    ));

    // Verify connectivity and clear webhook
    let me = bot.get_me().await?;
    info!("Connected as @{}", me.username());
    bot.delete_webhook().drop_pending_updates(true).await?;
    info!("Webhook cleared, starting polling...");

    dispatcher.dispatch().await;

    Ok(())
}
