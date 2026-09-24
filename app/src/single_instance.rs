use std::{
    ffi::OsString,
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};

use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    time::timeout,
};

const INSTANCE_ADDRESS: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 35_984);
const PROTOCOL_MAGIC: &str = "STREMIO_NATIVE/1";
const MAX_COMMAND_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppCommand {
    Activate,
    Open(String),
}

pub struct PrimaryInstance {
    pub initial_command: Option<AppCommand>,
    pub commands: UnboundedReceiver<AppCommand>,
    pub start_hidden: bool,
    pub listener_task: tokio::task::JoinHandle<()>,
}

pub enum InstanceStartup {
    Primary(PrimaryInstance),
    Forwarded,
}

#[derive(Default)]
struct StartupArguments {
    command: Option<String>,
    start_hidden: bool,
}

pub async fn acquire(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<InstanceStartup> {
    let arguments = parse_arguments(args);
    let initial_command = arguments.command.map(AppCommand::Open);

    let listener = match std::net::TcpListener::bind(INSTANCE_ADDRESS) {
        Ok(listener) => listener,
        Err(error) if error.kind() == ErrorKind::AddrInUse => {
            let command = initial_command.unwrap_or(AppCommand::Activate);
            forward(command).await?;
            return Ok(InstanceStartup::Forwarded);
        }
        Err(error) => {
            return Err(anyhow::anyhow!(
                "failed to reserve the Stremio instance endpoint {INSTANCE_ADDRESS}: {error}"
            ));
        }
    };

    listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(listener)?;
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    let listener_task = tokio::spawn(accept_commands(listener, commands_tx));

    Ok(InstanceStartup::Primary(PrimaryInstance {
        initial_command,
        commands: commands_rx,
        start_hidden: arguments.start_hidden,
        listener_task,
    }))
}

fn parse_arguments(args: impl IntoIterator<Item = OsString>) -> StartupArguments {
    let mut parsed = StartupArguments::default();
    for argument in args.into_iter().skip(1) {
        let Some(argument) = argument.to_str() else {
            continue;
        };
        if argument == "--start-hidden" {
            parsed.start_hidden = true;
        } else if parsed.command.is_none() && is_open_command(argument) {
            parsed.command = Some(argument.to_owned());
        }
    }
    parsed
}

fn is_open_command(value: &str) -> bool {
    value.starts_with("stremio:") || value.starts_with("magnet:")
}

async fn accept_commands(listener: TcpListener, commands: UnboundedSender<AppCommand>) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!(%error, "single-instance command listener stopped");
                return;
            }
        };
        if !peer.ip().is_loopback() {
            continue;
        }
        let commands = commands.clone();
        tokio::spawn(async move {
            if let Err(error) = receive_command(stream, commands).await {
                tracing::warn!(%error, %peer, "rejected single-instance command");
            }
        });
    }
}

async fn receive_command(
    mut stream: TcpStream,
    commands: UnboundedSender<AppCommand>,
) -> anyhow::Result<()> {
    let command = {
        let mut reader = BufReader::new(&mut stream);
        let magic = read_bounded_line(&mut reader).await?;
        if magic != PROTOCOL_MAGIC {
            return Err(anyhow::anyhow!("unexpected instance protocol"));
        }
        decode_command(&read_bounded_line(&mut reader).await?)?
    };

    commands
        .send(command)
        .map_err(|_| anyhow::anyhow!("primary instance is shutting down"))?;
    stream.write_all(b"OK\n").await?;
    stream.shutdown().await?;
    Ok(())
}

async fn forward(command: AppCommand) -> anyhow::Result<()> {
    timeout(Duration::from_secs(3), async move {
        let mut stream = TcpStream::connect(INSTANCE_ADDRESS).await.map_err(|error| {
            anyhow::anyhow!(
                "another process owns the Stremio instance endpoint, but it could not be reached: {error}"
            )
        })?;
        let encoded = encode_command(&command)?;
        stream
            .write_all(format!("{PROTOCOL_MAGIC}\n{encoded}\n").as_bytes())
            .await?;
        stream.flush().await?;

        let mut response = String::new();
        BufReader::new(&mut stream).read_line(&mut response).await?;
        if response.trim_end() != "OK" {
            return Err(anyhow::anyhow!(
                "another process owns the Stremio instance endpoint but did not accept the command"
            ));
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out contacting the existing Stremio instance"))?
}

fn encode_command(command: &AppCommand) -> anyhow::Result<String> {
    let encoded = match command {
        AppCommand::Activate => "activate".to_owned(),
        AppCommand::Open(value) => {
            if value.contains('\r')
                || value.contains('\n')
                || value.len() > MAX_COMMAND_BYTES.saturating_sub(5)
            {
                return Err(anyhow::anyhow!(
                    "deep-link command is too large or malformed"
                ));
            }
            format!("open:{value}")
        }
    };
    Ok(encoded)
}

fn decode_command(value: &str) -> anyhow::Result<AppCommand> {
    if value == "activate" {
        return Ok(AppCommand::Activate);
    }
    if let Some(value) = value.strip_prefix("open:")
        && is_open_command(value)
    {
        return Ok(AppCommand::Open(value.to_owned()));
    }
    Err(anyhow::anyhow!("unsupported instance command"))
}

async fn read_bounded_line(reader: &mut (impl AsyncBufRead + Unpin)) -> anyhow::Result<String> {
    let mut line = String::new();
    let read = reader
        .take((MAX_COMMAND_BYTES + 1) as u64)
        .read_line(&mut line)
        .await?;
    if read == 0 || read > MAX_COMMAND_BYTES || !line.ends_with('\n') {
        return Err(anyhow::anyhow!("invalid or oversized instance command"));
    }
    let trimmed_len = line.trim_end_matches(['\r', '\n']).len();
    line.truncate(trimmed_len);
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::{AppCommand, decode_command, encode_command, parse_arguments};

    #[test]
    fn startup_arguments_find_official_deep_link() {
        let args: [std::ffi::OsString; 3] = [
            "stremio-native".into(),
            "--start-hidden".into(),
            "stremio:///detail/movie/tt1254207".into(),
        ];
        let parsed = parse_arguments(args);

        assert!(parsed.start_hidden);
        assert_eq!(
            parsed.command.as_deref(),
            Some("stremio:///detail/movie/tt1254207")
        );
    }

    #[test]
    fn instance_protocol_round_trips_open_command() -> anyhow::Result<()> {
        let command = AppCommand::Open("magnet:?xt=urn:btih:abc".to_owned());
        assert_eq!(decode_command(&encode_command(&command)?)?, command);
        Ok(())
    }
}
