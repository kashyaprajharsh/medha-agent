//! `medha gateway`: set up messaging platforms and run the gateway that links them to Medha.

use crate::messaging_setup::Owner;
use anyhow::{Context, Result, bail};
use gateway::platforms::{self, Manifest};
use runtime::config;
use runtime::messaging::credential_id;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

const USAGE: &str = "usage: medha gateway setup [platform]\n       medha gateway run\n       medha gateway status\n       medha gateway install | uninstall | start | stop | restart";
/// What the gateway needs from the backend it joins.
const REQUIRED: &[&str] = &[
    "presentation-snapshot",
    "live-conversation",
    "replay-stream",
    "workspace-images",
];

pub(crate) async fn run(args: &[String]) -> Result<()> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["setup"] => setup(None).await,
        ["setup", name] => setup(Some(name)).await,
        ["run"] => serve().await,
        [] | ["status"] => status(),
        ["install"] => crate::gateway_service::install().map(|done| println!("{done}")),
        ["uninstall"] => crate::gateway_service::uninstall(),
        ["start"] => crate::gateway_service::start(),
        ["stop"] => crate::gateway_service::stop(),
        ["restart"] => crate::gateway_service::restart(),
        _ => bail!("{USAGE}"),
    }
}

async fn setup(name: Option<&str>) -> Result<()> {
    let manifest = match name {
        Some(name) => platforms::find(name)
            .with_context(|| format!("Medha does not know the platform {name}"))?,
        None => choose()?,
    };
    let Obtained {
        token,
        account,
        owner,
    } = obtain(manifest).await?;
    let folder = match config::load()?.unwrap_or_default().gateway.folder {
        Some(folder) => folder,
        None => {
            let here = std::env::current_dir()?;
            let answer = line(&format!(
                "Folder Medha works in when you message it [{}]: ",
                here.display()
            ))?;
            let chosen = if answer.trim().is_empty() {
                here
            } else {
                PathBuf::from(answer.trim())
            };
            runtime::messaging::admit_folder(&chosen)?
        }
    };
    let saved = crate::messaging_setup::save(
        manifest.name,
        &token,
        &account,
        &folder,
        owner.as_ref().map(|(owner, telegram)| (owner, telegram)),
    )
    .await?;
    let running = match &saved.problem {
        None => "It runs now, at login and after a crash.".to_owned(),
        Some(problem) => format!(
            "It could not be started at login: {problem}\nStart it yourself: medha gateway run"
        ),
    };
    match owner {
        Some(_) => println!(
            "\nSaved. You are paired, and Medha's notices come to this chat. Messages work in {}.\n{running}",
            folder.display()
        ),
        None => println!(
            "\nSaved. Messages from {} work in {}.\n{running}\n\nNext:\n  1. Message {account} from your phone. It answers with a code.\n  2. Let yourself in: medha pairing approve {} <code>",
            manifest.label,
            folder.display(),
            manifest.name
        ),
    }
    Ok(())
}

struct Obtained {
    token: String,
    account: String,
    /// The owner, with the connection to their new bot that saying "Paired" needs.
    owner: Option<(Owner, gateway::telegram::Telegram)>,
}

/// Telegram offers making the bot by scanning a code; every platform takes a pasted token.
async fn obtain(manifest: &Manifest) -> Result<Obtained> {
    if manifest.name == "telegram" && std::io::stdin().is_terminal() {
        println!(
            "Set up Telegram\n\n  1. Scan a code with your phone: Medha Setup makes a bot of your own (recommended)\n  2. Use a bot you made yourself with @BotFather\n"
        );
        if line("Choose [1]: ")?.trim() != "2" {
            return one_tap().await;
        }
    }
    println!("Set up {}\n\n{}\n", manifest.label, manifest.steps);
    let token = secret(&format!("{}: ", manifest.token))?;
    let account = platforms::check(manifest.name, &token).await?;
    println!("Connected to {account}.");
    Ok(Obtained {
        token,
        account,
        owner: None,
    })
}

async fn one_tap() -> Result<Obtained> {
    println!(
        "\nThe bot is yours and only you can message it. Telegram lets Medha Setup read the token of a bot it made for as long as that bot exists; choose 2 instead if no one else should.\n"
    );
    let waiting = gateway::one_tap::begin(&gateway::one_tap::service()).await?;
    if let Some(code) = gateway::one_tap::qr(&waiting.link) {
        println!("{code}");
    }
    println!(
        "Scan this with your phone, or open {}\nIn Telegram tap Start, then Create bot. Waiting for up to ten minutes…",
        waiting.link
    );
    let handed = waiting.wait().await?;
    println!("Created @{}.", handed.bot);
    let telegram = gateway::telegram::Telegram::connect(&handed.token).await?;
    println!("Now send any message to @{} from your phone.", handed.bot);
    let owner = tokio::time::timeout(
        std::time::Duration::from_secs(5 * 60),
        crate::messaging_setup::first_message(&telegram, handed.owner),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "No message reached @{} within five minutes. Run setup again.",
            handed.bot
        )
    })??;
    // The service names the creator and Telegram shows who wrote; the person confirms the
    // name, so a service that lied could not make someone else the owner.
    let answer = line(&format!(
        "Is this you: {} (Telegram {})? [Y/n] ",
        owner.name, owner.user
    ))?;
    if answer.trim().eq_ignore_ascii_case("n") {
        bail!(
            "Setup stopped. Delete @{} in @BotFather and run setup again.",
            handed.bot
        );
    }
    Ok(Obtained {
        token: handed.token,
        account: format!("@{}", handed.bot),
        owner: Some((owner, telegram)),
    })
}

fn choose() -> Result<&'static Manifest> {
    if let [only] = platforms::ALL {
        return Ok(only);
    }
    for (number, manifest) in platforms::ALL.iter().enumerate() {
        println!("  {}. {}", number + 1, manifest.label);
    }
    let answer = line("Platform number: ")?;
    answer
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|number| platforms::ALL.get(number.checked_sub(1)?))
        .context("choose one of the numbers shown")
}

async fn serve() -> Result<()> {
    let _running = gateway::hold_running()?;
    let _log = log()?;
    let cfg = config::load()?.unwrap_or_default();
    let folder = cfg
        .gateway
        .folder
        .clone()
        .context("Set up messaging first: medha gateway setup")?;
    // Canonical, so a file the agent made is told apart from one outside the folder.
    let folder = folder
        .canonicalize()
        .with_context(|| format!("the messaging folder {} is gone", folder.display()))?;
    let names: Vec<String> = cfg.gateway.enabled().map(str::to_owned).collect();
    if names.is_empty() {
        bail!("No messaging platform is set up. Run: medha gateway setup");
    }
    let backend = medha_client::Backend::for_surface(
        || std::env::current_exe().map_err(|error| error.to_string()),
        REQUIRED,
    );
    let mut routers = tokio::task::JoinSet::new();
    for name in &names {
        let token = config::load_key(&credential_id(name)).with_context(|| {
            format!("No token is saved for {name}. Run: medha gateway setup {name}")
        })?;
        let home = cfg
            .gateway
            .platforms
            .get(name)
            .and_then(|saved| saved.home())
            .map(str::to_owned);
        let (name, backend, folder) = (name.clone(), Arc::clone(&backend), folder.clone());
        routers.spawn(async move {
            let platform = connect(&name, &token).await?;
            gateway::Router::new(platform, backend, folder)?
                .home(home)
                .run()
                .await
        });
    }
    println!(
        "Medha gateway is running for {}, working in {}. Ctrl+C stops it.",
        names.join(", "),
        folder.display()
    );
    tokio::select! {
        _ = stopped() => Ok(()),
        Some(ended) = routers.join_next() => ended.context("the gateway stopped")?,
    }
}

/// Waits out a network that is not up yet, as at login; a refused token ends the gateway,
/// which then stays stopped until it is set up again.
async fn connect(name: &str, token: &str) -> Result<Arc<dyn gateway::platform::Platform>> {
    let mut wait = std::time::Duration::from_secs(10);
    loop {
        match platforms::connect(name, token).await {
            Ok(platform) => return Ok(platform),
            Err(error)
                if error
                    .downcast_ref::<gateway::platform::SetupProblem>()
                    .is_some() =>
            {
                return Err(error.context(format!(
                    "{name} refused the saved setup. Run: medha gateway setup {name}"
                )));
            }
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), ?wait, "gateway: connecting {name}");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(std::time::Duration::from_secs(60));
            }
        }
    }
}

/// Ctrl+C at a terminal, or the stop a service manager sends.
async fn stopped() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            },
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn status() -> Result<()> {
    let cfg = config::load()?.unwrap_or_default();
    if cfg.gateway.platforms.is_empty() {
        println!("Messaging is not set up. Run: medha gateway setup");
        if let Some(service) = crate::gateway_service::describe() {
            println!("Service: {service}");
        }
        return Ok(());
    }
    println!(
        "Gateway: {}",
        if gateway::running() {
            "running"
        } else {
            "not running (start it: medha gateway run)"
        }
    );
    println!(
        "Service: {}",
        crate::gateway_service::describe()
            .unwrap_or_else(|| "not installed (start at login: medha gateway install)".into())
    );
    if let Some(folder) = &cfg.gateway.folder {
        println!("Folder: {}", folder.display());
    }
    for (name, platform) in &cfg.gateway.platforms {
        let token = if config::load_key(&credential_id(name)).is_some() {
            "token saved"
        } else {
            "no token"
        };
        let home = match platform.home() {
            Some(chat) => format!("notices to chat {chat}"),
            None => "no home chat (send /sethome in the chat that should get notices)".into(),
        };
        println!(
            "  {name}: {}, {token}, {home}",
            if platform.enabled { "on" } else { "off" }
        );
    }
    Ok(())
}

/// A daily log in Medha's logs, as the backend keeps, for when no terminal is watching.
fn log() -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let logs = config::medha_home()?.join("logs");
    std::fs::create_dir_all(&logs).ok();
    let daily = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("gateway")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&logs)
        .context("could not open the gateway's log")?;
    let (writer, guard) = tracing_appender::non_blocking(daily);
    let _ = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    Ok(guard)
}

fn line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(answer.trim_end_matches(['\r', '\n']).to_owned())
}

/// Read without showing it; piped input is read as one line, so scripts can pass it.
fn secret(prompt: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        return line("").map(|text| text.trim().to_owned());
    }
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
    print!("{prompt}");
    std::io::stdout().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let mut typed = String::new();
    let outcome = loop {
        match read() {
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Enter => break Ok(()),
                KeyCode::Backspace => {
                    typed.pop();
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err(anyhow::anyhow!("cancelled"));
                }
                KeyCode::Char(c) => typed.push(c),
                _ => {}
            },
            Ok(Event::Paste(text)) => typed.push_str(&text),
            Ok(_) => {}
            Err(error) => break Err(error.into()),
        }
    };
    crossterm::terminal::disable_raw_mode()?;
    println!();
    outcome?;
    let typed = typed.trim().to_owned();
    if typed.is_empty() {
        bail!("nothing was entered");
    }
    Ok(typed)
}
